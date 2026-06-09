use crate::command::CommandRunner;
use crate::model::{Env, Session, SessionState, SessionType};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::path::{Path, PathBuf};

#[async_trait]
pub trait SessionBackend {
    async fn create(
        &self,
        env: &Env,
        session_id: &str,
        command: &[String],
        log_path: PathBuf,
    ) -> Result<Session>;
    async fn attach(&self, env: &Env, session_id: &str) -> Result<()>;
    async fn detach(&self, env: &Env, session_id: &str) -> Result<()>;
    async fn kill(&self, env: &Env, session_id: &str) -> Result<()>;
    async fn is_running(&self, env: &Env, session_id: &str) -> Result<bool>;
    async fn list(&self, env: &Env) -> Result<Vec<String>>;
    async fn logs(&self, env: &Env, session_id: &str, log_path: &Path) -> Result<String>;
    fn log_path(log_dir: &Path, session_id: &str) -> PathBuf
    where
        Self: Sized;
}

#[derive(Debug, Clone)]
pub struct TmuxSessionBackend {
    runner: CommandRunner,
}

impl Default for TmuxSessionBackend {
    fn default() -> Self {
        Self {
            runner: CommandRunner,
        }
    }
}

#[async_trait]
impl SessionBackend for TmuxSessionBackend {
    async fn create(
        &self,
        env: &Env,
        session_id: &str,
        command: &[String],
        log_path: PathBuf,
    ) -> Result<Session> {
        self.create(env, session_id, command, log_path).await
    }

    async fn attach(&self, env: &Env, session_id: &str) -> Result<()> {
        TmuxSessionBackend::attach(self, env, session_id).await
    }

    async fn detach(&self, env: &Env, session_id: &str) -> Result<()> {
        TmuxSessionBackend::detach(self, env, session_id).await
    }

    async fn kill(&self, env: &Env, session_id: &str) -> Result<()> {
        TmuxSessionBackend::kill(self, env, session_id).await
    }

    async fn is_running(&self, env: &Env, session_id: &str) -> Result<bool> {
        TmuxSessionBackend::is_running(self, env, session_id).await
    }

    async fn list(&self, env: &Env) -> Result<Vec<String>> {
        TmuxSessionBackend::list(self, env).await
    }

    async fn logs(&self, env: &Env, session_id: &str, log_path: &Path) -> Result<String> {
        TmuxSessionBackend::logs(self, env, session_id, log_path).await
    }

    fn log_path(log_dir: &Path, session_id: &str) -> PathBuf {
        Self::log_path(log_dir, session_id)
    }
}

impl TmuxSessionBackend {
    pub fn tmux_name(env_id: &str, session_id: &str) -> String {
        format!("af-{env_id}-{session_id}")
    }

    pub fn child_tmux_name(session_id: &str) -> String {
        session_id.to_string()
    }

    pub fn child_transcript_path(session_id: &str) -> String {
        format!("/var/log/agent-forkd/sessions/{session_id}.log")
    }

    pub fn child_create_command(session_id: &str, command: &[String]) -> String {
        let tmux = Self::child_tmux_name(session_id);
        let transcript = Self::child_transcript_path(session_id);
        let command = shell_join(command);
        format!(
            "mkdir -p /var/log/agent-forkd/sessions && tmux new-session -d -s {tmux} {command} && tmux pipe-pane -o -t {tmux} {pipe}",
            tmux = shell_quote(&tmux),
            command = shell_quote(&command),
            pipe = shell_quote(&format!("cat >> {transcript}")),
        )
    }

    fn machinectl_shell_args(machine: &str, command: &str) -> Vec<String> {
        vec![
            "shell".to_string(),
            machine.to_string(),
            "/bin/bash".to_string(),
            "-lc".to_string(),
            command.to_string(),
        ]
    }

    pub fn attach_args(env: &Env, session_id: &str) -> Vec<String> {
        let command = format!(
            "tmux attach-session -t {}",
            shell_quote(&Self::child_tmux_name(session_id))
        );
        Self::machinectl_shell_args(&env.machine_name, &command)
    }

    pub async fn create(
        &self,
        env: &Env,
        session_id: &str,
        command: &[String],
        log_path: PathBuf,
    ) -> Result<Session> {
        let inside = shell_join(command);
        let child_command = Self::child_create_command(session_id, command);
        self.runner
            .run_checked(
                "machinectl",
                Self::machinectl_shell_args(&env.machine_name, &child_command),
            )
            .await?;
        CommandRunner::append_to_file(
            &log_path,
            &format!(
                "created child tmux session {} in {}; child transcript {}\n",
                session_id,
                env.machine_name,
                Self::child_transcript_path(session_id)
            ),
        )
        .await?;
        Ok(Session {
            id: session_id.to_string(),
            env_id: env.id.clone(),
            command: inside,
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path,
        })
    }

    pub async fn attach(&self, env: &Env, session_id: &str) -> Result<()> {
        self.runner
            .run_checked("machinectl", Self::attach_args(env, session_id))
            .await?;
        Ok(())
    }

    pub async fn detach(&self, env: &Env, session_id: &str) -> Result<()> {
        let command = format!(
            "tmux detach-client -s {}",
            shell_quote(&Self::child_tmux_name(session_id))
        );
        let output = self
            .runner
            .run(
                "machinectl",
                Self::machinectl_shell_args(&env.machine_name, &command),
            )
            .await?;
        if output.status != 0 && !tmux_detach_reports_no_current_client(&output.stderr) {
            return Err(anyhow!(
                "failed to detach tmux session {session_id} in {}: {}{}",
                env.machine_name,
                output.stdout,
                output.stderr
            ));
        }
        Ok(())
    }

    pub async fn kill(&self, env: &Env, session_id: &str) -> Result<()> {
        let command = format!(
            "tmux kill-session -t {}",
            shell_quote(&Self::child_tmux_name(session_id))
        );
        self.runner
            .run_checked(
                "machinectl",
                Self::machinectl_shell_args(&env.machine_name, &command),
            )
            .await?;
        Ok(())
    }

    pub async fn is_running(&self, env: &Env, session_id: &str) -> Result<bool> {
        let command = format!(
            "tmux has-session -t {}",
            shell_quote(&Self::child_tmux_name(session_id))
        );
        let output = self
            .runner
            .run(
                "machinectl",
                Self::machinectl_shell_args(&env.machine_name, &command),
            )
            .await?;
        Ok(output.status == 0)
    }

    pub async fn list(&self, env: &Env) -> Result<Vec<String>> {
        let output = self
            .runner
            .run(
                "machinectl",
                Self::machinectl_shell_args(
                    &env.machine_name,
                    "tmux list-sessions -F '#{session_name}'",
                ),
            )
            .await?;
        if output.status != 0 {
            return Err(anyhow!(
                "failed to list tmux sessions in {}: {}",
                env.machine_name,
                output.stderr
            ));
        }
        Ok(parse_tmux_session_names(&output.stdout))
    }

    pub async fn logs(&self, env: &Env, session_id: &str, log_path: &Path) -> Result<String> {
        let child_path = Self::child_transcript_path(session_id);
        let command = format!("cat {} 2>/dev/null || true", shell_quote(&child_path));
        let output = self
            .runner
            .run(
                "machinectl",
                Self::machinectl_shell_args(&env.machine_name, &command),
            )
            .await?;
        let text = if output.status == 0 {
            output.stdout
        } else {
            match tokio::fs::read_to_string(log_path).await {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            }
        };
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(log_path, &text).await?;
        Ok(text)
    }

    pub fn log_path(log_dir: &Path, session_id: &str) -> PathBuf {
        log_dir.join(format!("{session_id}.log"))
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
                shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_tmux_session_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn tmux_detach_reports_no_current_client(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("no current client")
}

#[cfg(test)]
mod tests {
    use super::{
        parse_tmux_session_names, shell_join, shell_quote, tmux_detach_reports_no_current_client,
        TmuxSessionBackend,
    };

    #[test]
    fn tmux_name_scopes_by_env() {
        assert_eq!(
            TmuxSessionBackend::tmux_name("codex-1", "dev"),
            "af-codex-1-dev"
        );
    }

    #[test]
    fn child_create_command_uses_tmux_inside_child() {
        let command = TmuxSessionBackend::child_create_command(
            "dev",
            &["bash".to_string(), "-l".to_string()],
        );
        assert!(command.contains("tmux new-session -d -s 'dev'"));
        assert!(command.contains("tmux pipe-pane -o -t 'dev'"));
        assert!(command.contains("/var/log/agent-forkd/sessions/dev.log"));
        assert!(!command.contains("machinectl"));
    }

    #[test]
    fn session_command_display_preserves_shell_words() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "echo 'hello world'".into()]),
            "bash -lc 'echo '\\''hello world'\\'''"
        );
    }

    #[test]
    fn child_create_command_quotes_command_for_tmux() {
        let argv = ["bash".into(), "-lc".into(), "echo 'hello world'".into()];
        let rendered = shell_join(&argv);
        let command = TmuxSessionBackend::child_create_command("dev", &argv);

        assert!(command.contains(&shell_quote(&rendered)));
    }

    #[test]
    fn child_transcript_path_is_inside_child_not_agentfs() {
        let path = TmuxSessionBackend::child_transcript_path("codex");
        assert_eq!(path, "/var/log/agent-forkd/sessions/codex.log");
        assert!(!path.starts_with("/agentfs"));
    }

    #[test]
    fn attach_args_target_child_tmux() {
        use crate::model::{machine_name, Env, EnvState, Limits};
        use chrono::Utc;

        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Running,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: vec!["dev".to_string()],
        };
        let args = TmuxSessionBackend::attach_args(&env, "dev");
        assert_eq!(args[0], "shell");
        assert_eq!(args[1], "af-codex-1");
        assert!(args
            .last()
            .unwrap()
            .contains("tmux attach-session -t 'dev'"));
    }

    #[test]
    fn tmux_session_list_parser_ignores_empty_lines() {
        assert_eq!(
            parse_tmux_session_names("dev\n\n codex \n"),
            vec!["dev".to_string(), "codex".to_string()]
        );
    }

    #[test]
    fn detach_treats_absent_clients_as_already_detached() {
        assert!(tmux_detach_reports_no_current_client("no current client"));
        assert!(tmux_detach_reports_no_current_client("NO CURRENT CLIENT\n"));
        assert!(!tmux_detach_reports_no_current_client("no server running"));
    }
}
