use std::env;
use std::path::PathBuf;

use clap::{Arg, App, SubCommand, ArgMatches};

use crate::nix::deployment::{
    Deployment,
    Goal,
    Options,
    EvaluationNodeLimit,
    ParallelismLimit,
};
use crate::progress::SimpleProgressOutput;
use crate::nix::{NixError, NodeFilter};
use crate::util;

pub fn register_deploy_args<'a, 'b>(command: App<'a, 'b>) -> App<'a, 'b> {
    command
        .arg(Arg::with_name("eval-node-limit")
            .long("eval-node-limit")
            .value_name("LIMIT")
            .help("Evaluation node limit")
            .long_help(r#"Limits the maximum number of hosts to be evaluated at once.

The evaluation process is RAM-intensive. The default behavior is to limit the maximum number of host evaluated at the same time based on naive heuristics.

Set to 0 to disable the limit.
"#)
            .default_value("auto")
            .takes_value(true)
            .validator(|s| {
                if s == "auto" {
                    return Ok(());
                }

                match s.parse::<usize>() {
                    Ok(_) => Ok(()),
                    Err(_) => Err(String::from("The value must be a valid number")),
                }
            }))
        .arg(Arg::with_name("parallel")
            .short("p")
            .long("parallel")
            .value_name("LIMIT")
            .help("Deploy parallelism limit")
            .long_help(r#"Limits the maximum number of hosts to be deployed in parallel.

Set to 0 to disable parallemism limit.
"#)
            .default_value("10")
            .takes_value(true)
            .validator(|s| {
                match s.parse::<usize>() {
                    Ok(_) => Ok(()),
                    Err(_) => Err(String::from("The value must be a valid number")),
                }
            }))
        .arg(Arg::with_name("keep-result")
            .long("keep-result")
            .help("Create GC roots for built profiles")
            .long_help(r#"Create GC roots for built profiles.

The built system profiles will be added as GC roots so that they will not be removed by the garbage collector.
The links will be created under .gcroots in the directory the Hive configuration is located.
"#)
            .takes_value(false))
        .arg(Arg::with_name("verbose")
            .short("v")
            .long("verbose")
            .help("Be verbose")
            .long_help("Deactivates the progress spinner and prints every line of output.")
            .takes_value(false))
        .arg(Arg::with_name("no-keys")
            .long("no-keys")
            .help("Do not upload keys")
            .long_help(r#"Do not upload secret keys set in `deployment.keys`.

By default, Colmena will upload keys set in `deployment.keys` before deploying the new profile on a node.
To upload keys without building or deploying the rest of the configuration, use `colmena upload-keys`.
"#)
            .takes_value(false))
        .arg(Arg::with_name("no-substitutes")
            .long("no-substitutes")
            .help("Do not use substitutes")
            .long_help("Disables the use of substituters when copying closures to the remote host.")
            .takes_value(false))
        .arg(Arg::with_name("no-gzip")
            .long("no-gzip")
            .help("Do not use gzip")
            .long_help("Disables the use of gzip when copying closures to the remote host.")
            .takes_value(false))
        .arg(Arg::with_name("force-replace-unknown-profiles")
            .long("force-replace-unknown-profiles")
            .help("Ignore all targeted nodes deployment.replaceUnknownProfiles setting")
            .long_help(r#"If `deployment.replaceUnknownProfiles` is set for a target, using this switch
will treat deployment.replaceUnknownProfiles as though it was set true and perform unknown profile replacement."#)
            .takes_value(false))
}

pub fn subcommand() -> App<'static, 'static> {
    let command = SubCommand::with_name("apply")
        .about("Apply configurations on remote machines")
        .arg(Arg::with_name("goal")
            .help("Deployment goal")
            .long_help("Same as the targets for switch-to-configuration.\n\"push\" means only copying the closures to remote nodes.")
            .default_value("switch")
            .index(1)
            .possible_values(&["build", "push", "switch", "boot", "test", "dry-activate", "keys"]))
    ;
    let command = register_deploy_args(command);

    util::register_selector_args(command)
}

pub async fn run(_global_args: &ArgMatches<'_>, local_args: &ArgMatches<'_>) -> Result<(), NixError> {
    let hive = util::hive_from_args(local_args).await?;

    let ssh_config = env::var("SSH_CONFIG_FILE")
        .ok().map(PathBuf::from);

    let filter = if let Some(f) = local_args.value_of("on") {
        Some(NodeFilter::new(f)?)
    } else {
        None
    };

    let goal_arg = local_args.value_of("goal").unwrap();
    let goal = Goal::from_str(goal_arg).unwrap();

    let targets = hive.select_nodes(filter, ssh_config, goal.requires_target_host()).await?;
    let n_targets = targets.len();

    let mut output = SimpleProgressOutput::new(local_args.is_present("verbose"));
    let progress = output.get_sender();

    let mut deployment = Deployment::new(hive, targets, goal, progress);

    // FIXME: Configure limits
    let options = {
        let mut options = Options::default();
        options.set_substituters_push(!local_args.is_present("no-substitutes"));
        options.set_gzip(!local_args.is_present("no-gzip"));
        options.set_upload_keys(!local_args.is_present("no-keys"));
        options.set_force_replace_unknown_profiles(local_args.is_present("force-replace-unknown-profiles"));

        if local_args.is_present("keep-result") {
            options.set_create_gc_roots(true);
        }

        options
    };

    deployment.set_options(options);

    if local_args.is_present("no-keys") && goal == Goal::UploadKeys {
        log::error!("--no-keys cannot be used when the goal is to upload keys");
        quit::with_code(1);
    }

    let parallelism_limit = {
        let mut limit = ParallelismLimit::default();
        limit.set_apply_limit({
            let limit = local_args.value_of("parallel").unwrap().parse::<usize>().unwrap();
            if limit == 0 {
                n_targets
            } else {
                limit
            }
        });
        limit
    };

    let evaluation_node_limit = match local_args.value_of("eval-node-limit").unwrap() {
        "auto" => EvaluationNodeLimit::Heuristic,
        number => {
            let number = number.parse::<usize>().unwrap();
            if number == 0 {
                EvaluationNodeLimit::None
            } else {
                EvaluationNodeLimit::Manual(number)
            }
        }
    };

    deployment.set_parallelism_limit(parallelism_limit);
    deployment.set_evaluation_node_limit(evaluation_node_limit);

    let (deployment, output) = tokio::join!(
        deployment.execute(),
        output.run_until_completion(),
    );

    deployment?; output?;

    Ok(())
}
