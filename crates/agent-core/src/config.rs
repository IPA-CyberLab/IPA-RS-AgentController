use crate::model::Limits;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub agentfs: PathBuf,
    pub socket_path: PathBuf,
    pub default_profile: String,
    pub profiles: Vec<Profile>,
}

impl AgentConfig {
    pub fn new(agentfs: PathBuf) -> Self {
        let socket_path = agentfs.join("runtime/sockets/agent-forkd.sock");
        Self {
            agentfs,
            socket_path,
            default_profile: "privileged-dev".to_string(),
            profiles: vec![Profile::privileged_dev()],
        }
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub async fn load(path: &Path) -> Result<Self> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid config json {}", path.display()))?;
        for profile in &config.profiles {
            profile
                .limits
                .validate()
                .with_context(|| format!("invalid limits for profile {}", profile.name))?;
        }
        Ok(config)
    }

    pub async fn load_or_default(path: Option<&Path>, agentfs: PathBuf) -> Result<Self> {
        match path {
            Some(path) => Self::load(path).await,
            None => Ok(Self::new(agentfs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentConfig;

    #[tokio::test]
    async fn config_loads_from_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-forkd.json");
        tokio::fs::write(
            &path,
            r#"{
  "agentfs": "/tmp/agentfs",
  "socket_path": "/tmp/agentfs/runtime/sockets/agent-forkd.sock",
  "default_profile": "privileged-dev",
  "profiles": [
    {
      "name": "privileged-dev",
      "limits": {
        "cpu_max": "800%",
        "memory_max": "32G",
        "pids_max": 8192,
        "disk_max": "200G",
        "network": "private-nat",
        "idle_timeout": "0",
        "max_runtime": "0"
      }
    }
  ]
}"#,
        )
        .await
        .unwrap();

        let config = AgentConfig::load(&path).await.unwrap();
        assert_eq!(config.agentfs.to_string_lossy(), "/tmp/agentfs");
        assert_eq!(
            config.profile("privileged-dev").unwrap().limits.memory_max,
            "32G"
        );
    }

    #[tokio::test]
    async fn config_rejects_unknown_network_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-forkd.json");
        tokio::fs::write(
            &path,
            r#"{
  "agentfs": "/tmp/agentfs",
  "socket_path": "/tmp/agentfs/runtime/sockets/agent-forkd.sock",
  "default_profile": "privileged-dev",
  "profiles": [
    {
      "name": "privileged-dev",
      "limits": {
        "cpu_max": "800%",
        "memory_max": "32G",
        "pids_max": 8192,
        "disk_max": "200G",
        "network": "bridge",
        "idle_timeout": "0",
        "max_runtime": "0"
      }
    }
  ]
}"#,
        )
        .await
        .unwrap();

        assert!(AgentConfig::load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid limits for profile privileged-dev"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub limits: Limits,
}

impl Profile {
    pub fn privileged_dev() -> Self {
        Self {
            name: "privileged-dev".to_string(),
            limits: Limits::default(),
        }
    }
}
