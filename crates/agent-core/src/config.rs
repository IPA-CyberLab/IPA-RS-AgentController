use crate::model::{Limits, NetworkPolicy};
use crate::storage::validate_id;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
        config.validate()?;
        Ok(config)
    }

    pub async fn load_or_default(path: Option<&Path>, agentfs: PathBuf) -> Result<Self> {
        match path {
            Some(path) => Self::load(path).await,
            None => Ok(Self::new(agentfs)),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.default_profile)
            .with_context(|| format!("invalid default_profile {}", self.default_profile))?;
        let mut names = BTreeSet::new();
        for profile in &self.profiles {
            validate_id(&profile.name)
                .with_context(|| format!("invalid profile {}", profile.name))?;
            if !names.insert(profile.name.clone()) {
                return Err(anyhow!("duplicate profile {}", profile.name));
            }
            profile
                .limits
                .validate()
                .with_context(|| format!("invalid limits for profile {}", profile.name))?;
            profile
                .network_policy
                .validate()
                .with_context(|| format!("invalid network_policy for profile {}", profile.name))?;
        }
        if !names.contains(&self.default_profile) {
            return Err(anyhow!(
                "default_profile {} does not match any configured profile",
                self.default_profile
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AgentConfig;
    use crate::model::NetworkPolicy;

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
      },
      "network_policy": {
        "egress_proxy": "https://proxy.example.invalid:8443",
        "allowlist": ["api.openai.com", "github.com"]
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
        assert_eq!(
            config
                .profile("privileged-dev")
                .unwrap()
                .network_policy
                .allowlist,
            vec!["api.openai.com".to_string(), "github.com".to_string()]
        );
    }

    #[test]
    fn packaged_config_matches_runtime_parser() {
        let config: AgentConfig =
            serde_json::from_str(include_str!("../../../packaging/agent-forkd/config.json"))
                .unwrap();

        config.validate().unwrap();
        assert_eq!(config.agentfs.to_string_lossy(), "/agentfs");
        assert_eq!(
            config.socket_path.to_string_lossy(),
            "/agentfs/runtime/sockets/agent-forkd.sock"
        );
        assert_eq!(config.default_profile, "privileged-dev");
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(
            config.profile("privileged-dev").unwrap().limits,
            crate::model::Limits::default()
        );
        assert_eq!(
            config.profile("privileged-dev").unwrap().network_policy,
            NetworkPolicy::default()
        );
    }

    #[test]
    fn config_schema_exposes_network_policy_extension_points() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/config.schema.json")).unwrap();
        let properties = &schema["properties"]["profiles"]["items"]["properties"];
        assert!(
            properties.get("network_policy").is_some(),
            "config schema omitted network_policy"
        );
        assert!(
            properties["network_policy"]["properties"]
                .get("egress_proxy")
                .is_some(),
            "config schema omitted network_policy.egress_proxy"
        );
        assert!(
            properties["network_policy"]["properties"]
                .get("allowlist")
                .is_some(),
            "config schema omitted network_policy.allowlist"
        );
    }

    #[tokio::test]
    async fn config_accepts_missing_network_policy_for_backward_compatibility() {
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
        assert_eq!(
            config.profile("privileged-dev").unwrap().network_policy,
            NetworkPolicy::default()
        );
    }

    #[tokio::test]
    async fn config_accepts_partial_network_policy() {
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
      },
      "network_policy": {
        "egress_proxy": "https://proxy.example.invalid"
      }
    }
  ]
}"#,
        )
        .await
        .unwrap();

        let config = AgentConfig::load(&path).await.unwrap();
        let policy = &config.profile("privileged-dev").unwrap().network_policy;
        assert_eq!(
            policy.egress_proxy.as_deref(),
            Some("https://proxy.example.invalid")
        );
        assert!(policy.allowlist.is_empty());
    }

    #[tokio::test]
    async fn config_rejects_invalid_network_policy() {
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
      },
      "network_policy": {
        "egress_proxy": "socks5://proxy.example.invalid",
        "allowlist": []
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
            .contains("invalid network_policy for profile privileged-dev"));
    }

    #[tokio::test]
    async fn config_rejects_duplicate_allowlist_entries() {
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
      },
      "network_policy": {
        "allowlist": ["github.com", "github.com"]
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
            .contains("invalid network_policy for profile privileged-dev"));
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

    #[tokio::test]
    async fn config_rejects_missing_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-forkd.json");
        tokio::fs::write(
            &path,
            r#"{
  "agentfs": "/tmp/agentfs",
  "socket_path": "/tmp/agentfs/runtime/sockets/agent-forkd.sock",
  "default_profile": "missing",
  "profiles": [
    {
      "name": "privileged-dev",
      "limits": {
        "cpu_max": "400%",
        "memory_max": "16G",
        "pids_max": 4096,
        "disk_max": "100G",
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

        assert!(AgentConfig::load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match any configured profile"));
    }

    #[tokio::test]
    async fn config_rejects_duplicate_profile_names() {
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
        "cpu_max": "400%",
        "memory_max": "16G",
        "pids_max": 4096,
        "disk_max": "100G",
        "network": "private-nat",
        "idle_timeout": "0",
        "max_runtime": "0"
      }
    },
    {
      "name": "privileged-dev",
      "limits": {
        "cpu_max": "800%",
        "memory_max": "32G",
        "pids_max": 8192,
        "disk_max": "200G",
        "network": "private",
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
            .contains("duplicate profile privileged-dev"));
    }

    #[tokio::test]
    async fn config_rejects_unknown_fields() {
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
        "cpu_max": "400%",
        "memory_max": "16G",
        "pids_max": 4096,
        "disk_max": "100G",
        "network": "private-nat",
        "idle_timeout": "0",
        "max_runtime": "0",
        "typo": "ignored"
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
            .contains("invalid config json"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub limits: Limits,
    #[serde(default)]
    pub network_policy: NetworkPolicy,
}

impl Profile {
    pub fn privileged_dev() -> Self {
        Self {
            name: "privileged-dev".to_string(),
            limits: Limits::default(),
            network_policy: NetworkPolicy::default(),
        }
    }
}
