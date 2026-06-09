use crate::command::{CmdOutput, CommandRunner};
use crate::model::{unit_name, Env, EnvState};
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Nspawn {
    runner: CommandRunner,
    config_dir: PathBuf,
}

impl Default for Nspawn {
    fn default() -> Self {
        Self {
            runner: CommandRunner,
            config_dir: PathBuf::from("/etc/systemd/nspawn"),
        }
    }
}

impl Nspawn {
    pub fn config_text(env: &Env) -> String {
        format!(
            "[Exec]\nBoot=yes\nPrivateUsers=yes\nHostname={machine}\n\n[Files]\nReadOnly=no\n\n[Network]\nPrivate=yes\n",
            machine = env.machine_name
        )
    }

    pub async fn write_config(&self, env: &Env) -> Result<PathBuf> {
        let path = self.config_dir.join(format!("{}.nspawn", env.machine_name));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, Self::config_text(env)).await?;
        Ok(path)
    }

    pub async fn start(&self, env: &Env) -> Result<()> {
        self.write_config(env).await?;
        let unit = unit_name(&env.id);
        self.runner
            .run_checked(
                "systemd-run",
                vec![
                    format!("--unit={unit}"),
                    "--collect".to_string(),
                    "--property=Delegate=yes".to_string(),
                    format!("--property=CPUQuota={}", env.limits.cpu_max),
                    format!("--property=MemoryMax={}", env.limits.memory_max),
                    format!("--property=TasksMax={}", env.limits.pids_max),
                    "systemd-nspawn".to_string(),
                    "--machine".to_string(),
                    env.machine_name.clone(),
                    "--directory".to_string(),
                    env.rootfs_path.display().to_string(),
                    "--boot".to_string(),
                    "--private-users=yes".to_string(),
                    "--private-network".to_string(),
                    "--register=yes".to_string(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn stop(&self, machine_name: &str) -> Result<()> {
        self.runner
            .run_checked("machinectl", ["terminate", machine_name])
            .await?;
        Ok(())
    }

    pub async fn shell(&self, machine_name: &str) -> Result<()> {
        self.runner
            .run_checked("machinectl", ["shell", machine_name, "/bin/bash"])
            .await?;
        Ok(())
    }

    pub async fn exec(&self, env: &Env, command: &[String], log_path: &Path) -> Result<CmdOutput> {
        let mut args = vec![
            "shell".to_string(),
            env.machine_name.clone(),
            "/bin/bash".to_string(),
            "-lc".to_string(),
        ];
        args.push(shell_join(command));
        let output = self.runner.run("machinectl", args).await?;
        let log = format!(
            "$ {}\n[exit:{}]\n--- stdout ---\n{}--- stderr ---\n{}\n",
            shell_join(command),
            output.status,
            output.stdout,
            output.stderr
        );
        CommandRunner::append_to_file(log_path, &log).await?;
        Ok(output)
    }

    pub async fn refresh_state(&self, env: &mut Env) -> Result<()> {
        let output = self
            .runner
            .run(
                "machinectl",
                ["show", &env.machine_name, "-p", "State", "--value"],
            )
            .await?;
        env.state = if output.status == 0 && output.stdout.trim() == "running" {
            EnvState::Running
        } else {
            EnvState::Stopped
        };
        Ok(())
    }
}

fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_./:=+".contains(&b))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{machine_name, Limits};
    use chrono::Utc;

    #[test]
    fn nspawn_config_keeps_private_users_and_network() {
        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        let text = Nspawn::config_text(&env);
        assert!(text.contains("PrivateUsers=yes"));
        assert!(text.contains("Private=yes"));
        assert!(text.contains("Hostname=af-codex-1"));
    }

    #[test]
    fn shell_join_quotes_spaces() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "whoami && pwd".into()]),
            "bash -lc 'whoami && pwd'"
        );
    }
}
