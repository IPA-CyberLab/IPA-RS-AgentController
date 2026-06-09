use crate::model::{machine_name, Base, Env, Session};
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct Layout {
    pub agentfs: PathBuf,
}

impl Layout {
    pub fn new(agentfs: PathBuf) -> Self {
        Self { agentfs }
    }

    pub fn bases_dir(&self) -> PathBuf {
        self.agentfs.join("bases")
    }

    pub fn envs_dir(&self) -> PathBuf {
        self.agentfs.join("envs")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.agentfs.join("cache")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.agentfs.join("runtime")
    }

    pub fn socket_dir(&self) -> PathBuf {
        self.runtime_dir().join("sockets")
    }

    pub fn base_dir(&self, id: &str) -> PathBuf {
        self.bases_dir().join(id)
    }

    pub fn base_rootfs(&self, id: &str) -> PathBuf {
        self.base_dir(id).join("rootfs")
    }

    pub fn env_dir(&self, id: &str) -> PathBuf {
        self.envs_dir().join(id)
    }

    pub fn env_rootfs(&self, id: &str) -> PathBuf {
        self.env_dir(id).join("rootfs")
    }

    pub fn env_logs(&self, id: &str) -> PathBuf {
        self.env_dir(id).join("logs")
    }

    pub fn daemon_log(&self, id: &str) -> PathBuf {
        self.env_logs(id).join("agent-forkd.log")
    }

    pub fn lifecycle_log(&self, id: &str) -> PathBuf {
        self.env_logs(id).join("lifecycle.log")
    }

    pub fn nspawn_log(&self, id: &str) -> PathBuf {
        self.env_logs(id).join("nspawn.log")
    }

    pub fn session_logs(&self, env_id: &str) -> PathBuf {
        self.env_logs(env_id).join("sessions")
    }

    pub fn sessions_dir(&self, env_id: &str) -> PathBuf {
        self.env_dir(env_id).join("sessions")
    }

    pub async fn ensure_agentfs(&self) -> Result<()> {
        let dirs = [
            self.bases_dir(),
            self.envs_dir(),
            self.cache_dir().join("apt"),
            self.cache_dir().join("compiler"),
            self.cache_dir().join("package"),
            self.cache_dir().join("ddc"),
            self.runtime_dir().join("pty"),
            self.runtime_dir().join("machines"),
            self.socket_dir(),
        ];
        for dir in dirs {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    pub async fn write_base(&self, base: &Base) -> Result<()> {
        validate_id(&base.id)?;
        write_json(&self.base_dir(&base.id).join("manifest.json"), base).await
    }

    pub async fn read_base(&self, id: &str) -> Result<Base> {
        validate_id(id)?;
        read_json(&self.base_dir(id).join("manifest.json")).await
    }

    pub async fn write_env(&self, env: &Env) -> Result<()> {
        self.validate_env_metadata(env, Some(&env.id))?;
        write_json(&self.env_dir(&env.id).join("meta.json"), env).await
    }

    pub async fn read_env(&self, id: &str) -> Result<Env> {
        validate_id(id)?;
        let env: Env = read_json(&self.env_dir(id).join("meta.json")).await?;
        self.validate_env_metadata(&env, Some(id))?;
        Ok(env)
    }

    pub async fn write_session(&self, session: &Session) -> Result<()> {
        validate_id(&session.env_id)?;
        validate_id(&session.id)?;
        write_json(
            &self
                .sessions_dir(&session.env_id)
                .join(format!("{}.json", session.id)),
            session,
        )
        .await
    }

    pub async fn read_session(&self, env_id: &str, session_id: &str) -> Result<Session> {
        validate_id(env_id)?;
        validate_id(session_id)?;
        read_json(&self.sessions_dir(env_id).join(format!("{session_id}.json"))).await
    }

    pub async fn list_envs(&self) -> Result<Vec<Env>> {
        let mut envs: Vec<Env> = Vec::new();
        let mut entries = tokio::fs::read_dir(self.envs_dir()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path().join("meta.json");
            if path.exists() {
                let env: Env = read_json(&path).await?;
                self.validate_env_metadata(&env, None)?;
                envs.push(env);
            }
        }
        envs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(envs)
    }

    pub async fn list_sessions(&self, env_id: &str) -> Result<Vec<Session>> {
        validate_id(env_id)?;
        let dir = self.sessions_dir(env_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut sessions: Vec<Session> = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                sessions.push(read_json(&path).await?);
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    fn validate_env_metadata(&self, env: &Env, expected_id: Option<&str>) -> Result<()> {
        validate_id(&env.id)?;
        if let Some(expected_id) = expected_id {
            if env.id != expected_id {
                return Err(anyhow!(
                    "env metadata id {} does not match requested id {}",
                    env.id,
                    expected_id
                ));
            }
        }
        validate_id(&env.base_id)?;
        validate_id(&env.profile)?;
        env.limits.validate()?;
        for session_id in &env.sessions {
            validate_id(session_id)?;
        }
        let expected_machine = machine_name(&env.id);
        if env.machine_name != expected_machine {
            return Err(anyhow!(
                "env {} has machine_name {}, expected {}",
                env.id,
                env.machine_name,
                expected_machine
            ));
        }
        let expected_rootfs = self.env_rootfs(&env.id);
        if env.rootfs_path != expected_rootfs {
            return Err(anyhow!(
                "env {} has rootfs_path {}, expected {}",
                env.id,
                env.rootfs_path.display(),
                expected_rootfs.display()
            ));
        }
        Ok(())
    }
}

pub async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(&bytes).await?;
    file.write_all(b"\n").await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub async fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid json in {}", path.display()))
}

pub fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid id {id:?}; use ASCII letters, numbers, '-' or '_'"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{write_json, Layout};
    use crate::model::{machine_name, Env, EnvState, Limits, Session, SessionState, SessionType};
    use chrono::Utc;

    #[tokio::test]
    async fn storage_rejects_path_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().to_path_buf());

        assert!(layout.read_base("../base").await.is_err());
        assert!(layout.read_env("../env").await.is_err());
        assert!(layout.read_session("codex-1", "../session").await.is_err());
        assert!(layout.list_sessions("../env").await.is_err());
    }

    #[tokio::test]
    async fn storage_validates_ids_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().to_path_buf());
        let env = Env {
            id: "../env".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: dir.path().join("rootfs"),
            machine_name: "af-env".to_string(),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        assert!(layout.write_env(&env).await.is_err());

        let session = Session {
            id: "../session".to_string(),
            env_id: "codex-1".to_string(),
            command: "bash".to_string(),
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path: dir.path().join("session.log"),
        };
        assert!(layout.write_session(&session).await.is_err());
    }

    #[tokio::test]
    async fn storage_rejects_env_metadata_with_mismatched_identity() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().to_path_buf());
        let mut env = test_env(&layout, "codex-1");

        layout.write_env(&env).await.unwrap();
        assert_eq!(layout.read_env("codex-1").await.unwrap().id, "codex-1");

        env.machine_name = "af-other".to_string();
        assert!(layout.write_env(&env).await.is_err());

        env = test_env(&layout, "codex-1");
        env.rootfs_path = dir.path().join("envs/other/rootfs");
        assert!(layout.write_env(&env).await.is_err());
    }

    #[tokio::test]
    async fn storage_rejects_tampered_env_metadata_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().to_path_buf());
        let mut env = test_env(&layout, "codex-1");
        env.id = "other".to_string();
        write_json(&layout.env_dir("codex-1").join("meta.json"), &env)
            .await
            .unwrap();

        assert!(layout
            .read_env("codex-1")
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match requested id"));
    }

    fn test_env(layout: &Layout, id: &str) -> Env {
        Env {
            id: id.to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: layout.env_rootfs(id),
            machine_name: machine_name(id),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        }
    }
}
