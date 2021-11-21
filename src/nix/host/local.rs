use std::convert::TryInto;
use std::collections::HashMap;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use super::{CopyDirection, CopyOptions, Host, key_uploader};
use crate::nix::{StorePath, Profile, Goal, NixError, NixResult, NixCommand, Key, SYSTEM_PROFILE};
use crate::util::CommandExecution;
use crate::job::JobHandle;

/// The local machine running Colmena.
///
/// It may not be capable of realizing some derivations
/// (e.g., building Linux derivations on macOS).
#[derive(Debug)]
pub struct Local {
    job: Option<JobHandle>,
    nix_options: Vec<String>,
}

impl Local {
    pub fn new(nix_options: Vec<String>) -> Self {
        Self {
            job: None,
            nix_options,
        }
    }
}

#[async_trait]
impl Host for Local {
    async fn copy_closure(&mut self, _closure: &StorePath, _direction: CopyDirection, _options: CopyOptions) -> NixResult<()> {
        Ok(())
    }

    async fn realize_remote(&mut self, derivation: &StorePath) -> NixResult<Vec<StorePath>> {
        let mut command = Command::new("nix-store");

        command.args(self.nix_options.clone());
        command
            .arg("--no-gc-warning")
            .arg("--realise")
            .arg(derivation.as_path());

        let mut execution = CommandExecution::new(command);

        execution.set_job(self.job.clone());

        execution.run().await?;
        let (stdout, _) = execution.get_logs();

        stdout.unwrap().lines()
            .map(|p| p.to_string().try_into()).collect()
    }

    async fn upload_keys(&mut self, keys: &HashMap<String, Key>, require_ownership: bool) -> NixResult<()> {
        for (name, key) in keys {
            self.upload_key(&name, &key, require_ownership).await?;
        }

        Ok(())
    }

    async fn activate(&mut self, profile: &Profile, goal: Goal) -> NixResult<()> {
        if !goal.is_real_goal() {
            return Err(NixError::Unsupported);
        }

        if goal.should_switch_profile() {
            let path = profile.as_path().to_str().unwrap();
            Command::new("nix-env")
                .args(&["--profile", SYSTEM_PROFILE])
                .args(&["--set", path])
                .passthrough()
                .await?;
        }

        let activation_command = profile.activation_command(goal).unwrap();
        let mut command = Command::new(&activation_command[0]);
        command
            .args(&activation_command[1..]);

        let mut execution = CommandExecution::new(command);

        execution.set_job(self.job.clone());

        let result = execution.run().await;

        result
    }
    async fn active_derivation_known(&mut self) -> NixResult<bool> {
        Ok(true)
    }
}

impl Local {
    /// "Uploads" a single key.
    async fn upload_key(&mut self, name: &str, key: &Key, require_ownership: bool) -> NixResult<()> {
        if let Some(job) = &self.job {
            job.message(format!("Deploying key {}", name))?;
        }

        let dest_path = key.dest_dir().join(name);
        let key_script = format!("'{}'", key_uploader::generate_script(key, &dest_path, require_ownership));

        let mut command = Command::new("sh");

        command.args(&["-c", &key_script]);
        command.stdin(Stdio::piped());
        command.stderr(Stdio::piped());
        command.stdout(Stdio::piped());

        let uploader = command.spawn()?;
        key_uploader::feed_uploader(uploader, key, self.job.clone()).await
    }
}
