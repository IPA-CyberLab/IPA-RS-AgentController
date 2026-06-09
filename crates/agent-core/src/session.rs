use crate::command::CommandRunner;
use crate::model::{Env, Session, SessionState, SessionType};
use anyhow::Result;
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
    async fn attach(&self, env_id: &str, session_id: &str) -> Result<()>;
    async fn detach(&self, env_id: &str, session_id: &str) -> Result<()>;
    async fn kill(&self, env_id: &str, session_id: &str) -> Result<()>;
    async fn is_running(&self, env_id: &str, session_id: &str) -> Result<bool>;
    async fn list(&self, env_id: &str) -> Result<Vec<String>>;
    async fn logs(&self, log_path: &Path) -> Result<String>;
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

    async fn attach(&self, env_id: &str, session_id: &str) -> Result<()> {
        TmuxSessionBackend::attach(self, env_id, session_id).await
    }

    async fn detach(&self, env_id: &str, session_id: &str) -> Result<()> {
        TmuxSessionBackend::detach(self, env_id, session_id).await
    }

    async fn kill(&self, env_id: &str, session_id: &str) -> Result<()> {
        TmuxSessionBackend::kill(self, env_id, session_id).await
    }

    async fn is_running(&self, env_id: &str, session_id: &str) -> Result<bool> {
        TmuxSessionBackend::is_running(self, env_id, session_id).await
    }

    async fn list(&self, env_id: &str) -> Result<Vec<String>> {
        TmuxSessionBackend::list(self, env_id).await
    }

    async fn logs(&self, log_path: &Path) -> Result<String> {
        TmuxSessionBackend::logs(self, log_path).await
    }

    fn log_path(log_dir: &Path, session_id: &str) -> PathBuf {
        Self::log_path(log_dir, session_id)
    }
}

impl TmuxSessionBackend {
    pub fn tmux_name(env_id: &str, session_id: &str) -> String {
        format!("af-{env_id}-{session_id}")
    }

    pub async fn create(
        &self,
        env: &Env,
        session_id: &str,
        command: &[String],
        log_path: PathBuf,
    ) -> Result<Session> {
        let tmux = Self::tmux_name(&env.id, session_id);
        let inside = command.join(" ");
        let wrapped = format!(
            "machinectl shell {machine} /bin/bash -lc {cmd:?} 2>&1 | tee -a {log}",
            machine = env.machine_name,
            cmd = inside,
            log = log_path.display()
        );
        self.runner
            .run_checked("tmux", ["new-session", "-d", "-s", &tmux, &wrapped])
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

    pub async fn attach(&self, env_id: &str, session_id: &str) -> Result<()> {
        self.runner
            .run_checked(
                "tmux",
                ["attach-session", "-t", &Self::tmux_name(env_id, session_id)],
            )
            .await?;
        Ok(())
    }

    pub async fn detach(&self, env_id: &str, session_id: &str) -> Result<()> {
        self.runner
            .run_checked(
                "tmux",
                ["detach-client", "-s", &Self::tmux_name(env_id, session_id)],
            )
            .await?;
        Ok(())
    }

    pub async fn kill(&self, env_id: &str, session_id: &str) -> Result<()> {
        self.runner
            .run_checked(
                "tmux",
                ["kill-session", "-t", &Self::tmux_name(env_id, session_id)],
            )
            .await?;
        Ok(())
    }

    pub async fn is_running(&self, env_id: &str, session_id: &str) -> Result<bool> {
        let output = self
            .runner
            .run(
                "tmux",
                ["has-session", "-t", &Self::tmux_name(env_id, session_id)],
            )
            .await?;
        Ok(output.status == 0)
    }

    pub async fn list(&self, env_id: &str) -> Result<Vec<String>> {
        let output = self
            .runner
            .run("tmux", ["list-sessions", "-F", "#{session_name}"])
            .await?;
        if output.status != 0 {
            return Ok(Vec::new());
        }
        let prefix = format!("af-{env_id}-");
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .map(ToOwned::to_owned)
            .collect())
    }

    pub async fn logs(&self, log_path: &Path) -> Result<String> {
        match tokio::fs::read_to_string(log_path).await {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn log_path(log_dir: &Path, session_id: &str) -> PathBuf {
        log_dir.join(format!("{session_id}.log"))
    }
}

#[cfg(test)]
mod tests {
    use super::TmuxSessionBackend;

    #[test]
    fn tmux_name_scopes_by_env() {
        assert_eq!(
            TmuxSessionBackend::tmux_name("codex-1", "dev"),
            "af-codex-1-dev"
        );
    }
}
