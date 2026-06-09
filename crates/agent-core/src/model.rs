use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Base {
    pub id: String,
    pub rootfs_path: PathBuf,
    pub readonly: bool,
    pub created_at: DateTime<Utc>,
    pub source: String,
    pub dpkg_manifest: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Env {
    pub id: String,
    pub base_id: String,
    pub rootfs_path: PathBuf,
    pub machine_name: String,
    pub state: EnvState,
    pub profile: String,
    pub created_at: DateTime<Utc>,
    pub limits: Limits,
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvState {
    Created,
    Running,
    Stopped,
    Failed,
    QuotaExceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    pub cpu_max: String,
    pub memory_max: String,
    pub pids_max: u32,
    pub disk_max: String,
    pub network: String,
    pub idle_timeout: String,
    pub max_runtime: String,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_max: "400%".to_string(),
            memory_max: "16G".to_string(),
            pids_max: 4096,
            disk_max: "100G".to_string(),
            network: "private-nat".to_string(),
            idle_timeout: "0".to_string(),
            max_runtime: "0".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LimitOverrides {
    pub cpu_max: Option<String>,
    pub memory_max: Option<String>,
    pub pids_max: Option<u32>,
    pub disk_max: Option<String>,
    pub network: Option<String>,
    pub idle_timeout: Option<String>,
    pub max_runtime: Option<String>,
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.network.as_str(), "private-nat" | "private") {
            bail!(
                "unsupported network mode {}; use private-nat or private",
                self.network
            );
        }
        Ok(())
    }

    pub fn with_overrides(mut self, overrides: LimitOverrides) -> Self {
        if let Some(value) = overrides.cpu_max {
            self.cpu_max = value;
        }
        if let Some(value) = overrides.memory_max {
            self.memory_max = value;
        }
        if let Some(value) = overrides.pids_max {
            self.pids_max = value;
        }
        if let Some(value) = overrides.disk_max {
            self.disk_max = value;
        }
        if let Some(value) = overrides.network {
            self.network = value;
        }
        if let Some(value) = overrides.idle_timeout {
            self.idle_timeout = value;
        }
        if let Some(value) = overrides.max_runtime {
            self.max_runtime = value;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn limits_accept_supported_network_modes() {
        Limits {
            network: "private-nat".to_string(),
            ..Limits::default()
        }
        .validate()
        .unwrap();

        Limits {
            network: "private".to_string(),
            ..Limits::default()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn limits_reject_unknown_network_modes() {
        let limits = Limits {
            network: "bridge".to_string(),
            ..Limits::default()
        };

        assert!(limits
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported network mode"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub env_id: String,
    pub command: String,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    #[serde(rename = "type")]
    pub session_type: SessionType,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Pty,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvStatus {
    pub env: Env,
    pub disk_used: Option<String>,
}

pub fn machine_name(env_id: &str) -> String {
    format!("af-{}", env_id)
}

pub fn unit_name(env_id: &str) -> String {
    format!("agent-forkd-{}.service", env_id)
}
