use crate::command::CommandRunner;
use crate::model::{Env, Session, SessionState, SessionType};
use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

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
