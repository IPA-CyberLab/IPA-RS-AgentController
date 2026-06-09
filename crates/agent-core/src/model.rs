use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Base {
    pub id: String,
    pub rootfs_path: PathBuf,
    pub readonly: bool,
    pub created_at: DateTime<Utc>,
    pub source: String,
    pub dpkg_manifest: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        validate_cpu_max(&self.cpu_max)?;
        validate_size_limit("memory_max", &self.memory_max)?;
        validate_size_limit("disk_max", &self.disk_max)?;
        validate_duration_limit("idle_timeout", &self.idle_timeout)?;
        validate_duration_limit("max_runtime", &self.max_runtime)?;
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

fn validate_cpu_max(value: &str) -> Result<()> {
    let value = value.trim();
    if is_unlimited(value) {
        return Ok(());
    }
    let Some(percent) = value.strip_suffix('%') else {
        bail!("cpu_max must be a positive percentage or 0/unlimited");
    };
    validate_positive_decimal("cpu_max", percent)
}

fn validate_size_limit(name: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if is_unlimited(value) {
        return Ok(());
    }
    let Some((number, unit)) = split_number_unit(value) else {
        bail!("{name} must be a positive size with a unit or 0/unlimited");
    };
    if !matches!(
        unit.to_ascii_lowercase().as_str(),
        "b" | "k" | "kb" | "m" | "mb" | "g" | "gb" | "t" | "tb"
    ) {
        bail!("{name} has unsupported unit {unit}");
    }
    validate_positive_decimal(name, number)
}

fn validate_duration_limit(name: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if is_unlimited(value) {
        return Ok(());
    }
    let Some((number, unit)) = split_number_unit(value) else {
        bail!("{name} must be a positive duration with a unit or 0/unlimited");
    };
    if !matches!(
        unit.to_ascii_lowercase().as_str(),
        "s" | "sec" | "m" | "min" | "h" | "hr" | "d" | "day"
    ) {
        bail!("{name} has unsupported unit {unit}");
    }
    validate_positive_decimal(name, number)
}

fn split_number_unit(value: &str) -> Option<(&str, &str)> {
    let split_at = value.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = value.split_at(split_at);
    if number.is_empty() || unit.is_empty() || !unit.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((number, unit))
}

fn validate_positive_decimal(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.parse::<f64>().map_or(true, |number| number <= 0.0)
    {
        bail!("{name} must be positive");
    }
    Ok(())
}

fn is_unlimited(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "unlimited" | "infinity" | "none"
    )
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

    #[test]
    fn limits_reject_malformed_resource_values() {
        for (field, error) in [
            ("cpu_max", "cpu_max must be a positive percentage"),
            ("memory_max", "memory_max must be a positive size"),
            ("disk_max", "disk_max has unsupported unit"),
            ("idle_timeout", "idle_timeout must be positive"),
            ("max_runtime", "max_runtime has unsupported unit"),
        ] {
            let mut limits = Limits::default();
            match field {
                "cpu_max" => limits.cpu_max = "four".to_string(),
                "memory_max" => limits.memory_max = "16".to_string(),
                "disk_max" => limits.disk_max = "100Q".to_string(),
                "idle_timeout" => limits.idle_timeout = "-1s".to_string(),
                "max_runtime" => limits.max_runtime = "1fortnight".to_string(),
                _ => unreachable!(),
            }
            assert!(
                limits.validate().unwrap_err().to_string().contains(error),
                "expected {field} error to contain {error:?}"
            );
        }
    }

    #[test]
    fn limits_accept_unlimited_resource_values() {
        let limits = Limits {
            cpu_max: "unlimited".to_string(),
            memory_max: "infinity".to_string(),
            disk_max: "none".to_string(),
            idle_timeout: "0".to_string(),
            max_runtime: "0".to_string(),
            ..Limits::default()
        };

        limits.validate().unwrap();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
