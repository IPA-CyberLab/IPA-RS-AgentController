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
