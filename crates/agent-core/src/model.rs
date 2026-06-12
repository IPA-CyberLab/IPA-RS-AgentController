use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Base {
    pub id: String,
    #[serde(default)]
    pub backend: RootfsBackend,
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
    #[serde(default)]
    pub backend: RootfsBackend,
    pub rootfs_path: PathBuf,
    pub machine_name: String,
    pub state: EnvState,
    pub profile: String,
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub last_active_at: DateTime<Utc>,
    pub limits: Limits,
    #[serde(default)]
    pub network_policy: NetworkPolicy,
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootfsBackend {
    #[default]
    Btrfs,
    Overlay,
    ApfsClone,
    WindowsBlockClone,
}

impl RootfsBackend {
    pub fn native_clone_for_current_os() -> Option<Self> {
        native_clone_backend()
    }
}

#[cfg(target_os = "macos")]
fn native_clone_backend() -> Option<RootfsBackend> {
    Some(RootfsBackend::ApfsClone)
}

#[cfg(target_os = "windows")]
fn native_clone_backend() -> Option<RootfsBackend> {
    Some(RootfsBackend::WindowsBlockClone)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn native_clone_backend() -> Option<RootfsBackend> {
    None
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
            network: "host".to_string(),
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    pub egress_proxy: Option<String>,
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl NetworkPolicy {
    pub fn validate(&self) -> Result<()> {
        if let Some(proxy) = &self.egress_proxy {
            if !(proxy.starts_with("http://") || proxy.starts_with("https://")) {
                bail!("egress_proxy must start with http:// or https://");
            }
        }
        let mut seen = BTreeSet::new();
        for entry in &self.allowlist {
            if entry.trim().is_empty() {
                bail!("allowlist entries must not be empty");
            }
            if !seen.insert(entry) {
                bail!("allowlist entries must be unique");
            }
        }
        Ok(())
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.network.as_str(), "host" | "bridge" | "none") {
            bail!(
                "unsupported network mode {}; use host, bridge, or none",
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

    pub fn network_allows_host_access(&self) -> bool {
        matches!(self.network.as_str(), "host" | "bridge")
    }

    pub fn network_uses_bridge(&self) -> bool {
        self.network == "bridge"
    }

    pub fn network_is_disabled(&self) -> bool {
        self.network == "none"
    }

    pub fn idle_timeout_duration(&self) -> Result<Option<Duration>> {
        duration_limit("idle_timeout", &self.idle_timeout)
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
    duration_limit(name, value).map(|_| ())
}

fn duration_limit(name: &str, value: &str) -> Result<Option<Duration>> {
    let value = value.trim();
    if is_unlimited(value) {
        return Ok(None);
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
    validate_positive_decimal(name, number)?;
    let value = number
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("{name} must be positive"))?;
    let seconds = match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" => value,
        "m" | "min" => value * 60.0,
        "h" | "hr" => value * 60.0 * 60.0,
        "d" | "day" => value * 60.0 * 60.0 * 24.0,
        _ => unreachable!("duration unit was validated above"),
    };
    let duration = StdDuration::from_secs_f64(seconds);
    Ok(Some(Duration::from_std(duration).map_err(|_| {
        anyhow::anyhow!("{name} duration is too large")
    })?))
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
    use super::{
        Base, Env, EnvState, LimitOverrides, Limits, NetworkPolicy, RootfsBackend, Session,
        SessionState, SessionType,
    };
    use chrono::Utc;
    use serde_json::Value;
    use std::path::PathBuf;

    #[test]
    fn limits_accept_supported_network_modes() {
        for network in ["host", "bridge", "none"] {
            Limits {
                network: network.to_string(),
                ..Limits::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn limits_reject_unknown_network_modes() {
        let limits = Limits {
            network: "private-nat".to_string(),
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

    #[test]
    fn limit_overrides_replace_only_specified_fields() {
        let limits = Limits::default().with_overrides(LimitOverrides {
            cpu_max: Some("800%".to_string()),
            memory_max: Some("32G".to_string()),
            pids_max: Some(8192),
            disk_max: Some("200G".to_string()),
            network: Some("none".to_string()),
            idle_timeout: Some("30m".to_string()),
            max_runtime: Some("6h".to_string()),
        });

        assert_eq!(limits.cpu_max, "800%");
        assert_eq!(limits.memory_max, "32G");
        assert_eq!(limits.pids_max, 8192);
        assert_eq!(limits.disk_max, "200G");
        assert_eq!(limits.network, "none");
        assert_eq!(limits.idle_timeout, "30m");
        assert_eq!(limits.max_runtime, "6h");
    }

    #[test]
    fn idle_timeout_duration_parses_supported_units() {
        let mut limits = Limits {
            idle_timeout: "1.5m".to_string(),
            ..Limits::default()
        };
        assert_eq!(
            limits
                .idle_timeout_duration()
                .unwrap()
                .unwrap()
                .num_seconds(),
            90
        );

        limits.idle_timeout = "0".to_string();
        assert!(limits.idle_timeout_duration().unwrap().is_none());
    }

    #[test]
    fn empty_limit_overrides_preserve_profile_defaults() {
        assert_eq!(
            Limits::default().with_overrides(LimitOverrides::default()),
            Limits::default()
        );
    }

    #[test]
    fn metadata_schemas_require_model_fields() {
        assert_schema_required_fields(
            include_str!("../../../schemas/base.schema.json"),
            &[
                "id",
                "backend",
                "rootfs_path",
                "readonly",
                "created_at",
                "source",
                "dpkg_manifest",
            ],
        );
        assert_schema_required_fields(
            include_str!("../../../schemas/env.schema.json"),
            &[
                "id",
                "base_id",
                "backend",
                "rootfs_path",
                "machine_name",
                "state",
                "profile",
                "created_at",
                "last_active_at",
                "limits",
                "network_policy",
                "sessions",
            ],
        );
        assert_schema_required_fields(
            include_str!("../../../schemas/session.schema.json"),
            &[
                "id",
                "env_id",
                "command",
                "state",
                "created_at",
                "type",
                "log_path",
            ],
        );
        assert_schema_array(
            include_str!("../../../schemas/env.schema.json"),
            &["properties", "network_policy", "required"],
            &["egress_proxy", "allowlist"],
        );
    }

    #[test]
    fn metadata_schema_states_match_serde_wire_values() {
        assert_schema_enum_values(
            include_str!("../../../schemas/env.schema.json"),
            &["properties", "state", "enum"],
            &["created", "running", "stopped", "failed", "quota_exceeded"],
        );
        assert_schema_enum_values(
            include_str!("../../../schemas/base.schema.json"),
            &["properties", "backend", "enum"],
            &["btrfs", "overlay", "apfs_clone", "windows_block_clone"],
        );
        assert_schema_enum_values(
            include_str!("../../../schemas/env.schema.json"),
            &["properties", "backend", "enum"],
            &["btrfs", "overlay", "apfs_clone", "windows_block_clone"],
        );
        assert_schema_enum_values(
            include_str!("../../../schemas/session.schema.json"),
            &["properties", "state", "enum"],
            &["running", "stopped", "failed"],
        );
        assert_schema_enum_values(
            include_str!("../../../schemas/session.schema.json"),
            &["properties", "type", "enum"],
            &["pty"],
        );

        assert_eq!(
            serde_json::to_value(EnvState::QuotaExceeded).unwrap(),
            Value::String("quota_exceeded".to_string())
        );
        assert_eq!(
            serde_json::to_value(SessionState::Running).unwrap(),
            Value::String("running".to_string())
        );
        assert_eq!(
            serde_json::to_value(SessionType::Pty).unwrap(),
            Value::String("pty".to_string())
        );
    }

    #[test]
    fn native_clone_backend_matches_current_os() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            RootfsBackend::native_clone_for_current_os(),
            Some(RootfsBackend::ApfsClone)
        );
        #[cfg(target_os = "windows")]
        assert_eq!(
            RootfsBackend::native_clone_for_current_os(),
            Some(RootfsBackend::WindowsBlockClone)
        );
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(RootfsBackend::native_clone_for_current_os(), None);
    }

    #[test]
    fn metadata_model_serializes_schema_fields() {
        let base = Base {
            id: "base-001".to_string(),
            backend: RootfsBackend::Btrfs,
            rootfs_path: PathBuf::from("/agentfs/bases/base-001/rootfs"),
            readonly: true,
            created_at: Utc::now(),
            source: "current-project-vm".to_string(),
            dpkg_manifest: PathBuf::from("/agentfs/bases/base-001/dpkg.list"),
        };
        assert_json_object_fields(
            serde_json::to_value(base).unwrap(),
            &[
                "id",
                "backend",
                "rootfs_path",
                "readonly",
                "created_at",
                "source",
                "dpkg_manifest",
            ],
        );

        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            backend: RootfsBackend::Btrfs,
            rootfs_path: PathBuf::from("/agentfs/envs/codex-1/rootfs"),
            machine_name: "af-codex-1".to_string(),
            state: EnvState::Running,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            network_policy: NetworkPolicy::default(),
            limits: Limits::default(),
            sessions: vec!["dev".to_string()],
        };
        assert_json_object_fields(
            serde_json::to_value(env).unwrap(),
            &[
                "id",
                "base_id",
                "backend",
                "rootfs_path",
                "machine_name",
                "state",
                "profile",
                "created_at",
                "last_active_at",
                "limits",
                "network_policy",
                "sessions",
            ],
        );

        let session = Session {
            id: "dev".to_string(),
            env_id: "codex-1".to_string(),
            command: "bash".to_string(),
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path: PathBuf::from("/agentfs/envs/codex-1/logs/sessions/dev.log"),
        };
        assert_json_object_fields(
            serde_json::to_value(session).unwrap(),
            &[
                "id",
                "env_id",
                "command",
                "state",
                "created_at",
                "type",
                "log_path",
            ],
        );
    }

    fn assert_schema_required_fields(schema: &str, expected: &[&str]) {
        assert_schema_array(schema, &["required"], expected);
    }

    fn assert_schema_enum_values(schema: &str, path: &[&str], expected: &[&str]) {
        assert_schema_array(schema, path, expected);
    }

    fn assert_schema_array(schema: &str, path: &[&str], expected: &[&str]) {
        let value: Value = serde_json::from_str(schema).unwrap();
        let mut current = &value;
        for segment in path {
            current = current
                .get(*segment)
                .unwrap_or_else(|| panic!("schema path {path:?} missing {segment}"));
        }
        let actual = current
            .as_array()
            .unwrap_or_else(|| panic!("schema path {path:?} was not an array"))
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn assert_json_object_fields(value: Value, expected: &[&str]) {
        let object = value.as_object().unwrap();
        let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
        let mut expected = expected.to_vec();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
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
