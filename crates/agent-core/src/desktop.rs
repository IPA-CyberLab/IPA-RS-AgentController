use crate::command::{CmdOutput, CommandRunner};
use crate::config::AgentConfig;
use crate::model::{machine_name, Base, Env, EnvState, EnvStatus, LimitOverrides, RootfsBackend};
use crate::reflink;
use crate::storage::{validate_id, write_text_file, Layout};
use anyhow::{anyhow, Result};
use chrono::Utc;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DesktopService {
    pub config: AgentConfig,
    layout: Layout,
    runner: CommandRunner,
}

impl DesktopService {
    pub fn new(config: AgentConfig) -> Self {
        let layout = Layout::new(config.agentfs.clone());
        Self {
            config,
            layout,
            runner: CommandRunner,
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.layout.ensure_agentfs().await
    }

    pub async fn base_freeze(&self, name: &str, from: &Path) -> Result<Base> {
        let backend = RootfsBackend::native_clone_for_current_os().ok_or_else(|| {
            anyhow!("native desktop backend is supported only on macOS and Windows")
        })?;
        self.base_freeze_with_backend(name, from, backend).await
    }

    pub async fn base_freeze_with_backend(
        &self,
        name: &str,
        from: &Path,
        backend: RootfsBackend,
    ) -> Result<Base> {
        validate_id(name)?;
        if !matches!(
            backend,
            RootfsBackend::ApfsClone | RootfsBackend::WindowsBlockClone
        ) {
            return Err(anyhow!(
                "backend {backend:?} is not a desktop clone backend"
            ));
        }
        let base_dir = self.layout.base_dir(name);
        if base_dir.exists() {
            return Err(anyhow!("base {name} already exists"));
        }
        tokio::fs::create_dir_all(&base_dir).await?;
        let rootfs = self.layout.base_rootfs(name);
        if let Err(error) = reflink::clone_tree(from, &rootfs) {
            let _ = remove_dir_all_if_exists(&base_dir).await;
            return Err(error);
        }
        let created_at = Utc::now();
        let dpkg_manifest = base_dir.join("dpkg.list");
        write_text_file(&dpkg_manifest, "").await?;
        write_text_file(&base_dir.join("created_at"), created_at.to_rfc3339()).await?;
        let base = Base {
            id: name.to_string(),
            backend,
            rootfs_path: rootfs,
            readonly: true,
            created_at,
            source: from.display().to_string(),
            dpkg_manifest,
        };
        self.layout.write_base(&base).await?;
        Ok(base)
    }

    pub async fn env_create(
        &self,
        id: &str,
        base_id: &str,
        profile_name: &str,
        limit_overrides: LimitOverrides,
    ) -> Result<Env> {
        validate_id(id)?;
        validate_id(base_id)?;
        let profile = self
            .config
            .profile(profile_name)
            .ok_or_else(|| anyhow!("unknown profile {profile_name}"))?;
        let base = self.layout.read_base(base_id).await?;
        if !matches!(
            base.backend,
            RootfsBackend::ApfsClone | RootfsBackend::WindowsBlockClone
        ) {
            return Err(anyhow!("base {base_id} is not a desktop native base"));
        }
        let env_dir = self.layout.env_dir(id);
        if env_dir.exists() {
            return Err(anyhow!("env {id} already exists"));
        }
        tokio::fs::create_dir_all(&env_dir).await?;
        let rootfs = self.layout.env_rootfs(id);
        if let Err(error) = reflink::clone_tree(&base.rootfs_path, &rootfs) {
            let _ = remove_dir_all_if_exists(&env_dir).await;
            return Err(error);
        }
        let limits = profile.limits.clone().with_overrides(limit_overrides);
        limits.validate()?;
        tokio::fs::create_dir_all(self.layout.session_logs(id)).await?;
        tokio::fs::create_dir_all(self.layout.sessions_dir(id)).await?;
        tokio::fs::create_dir_all(env_dir.join("exports")).await?;
        let env = Env {
            id: id.to_string(),
            base_id: base_id.to_string(),
            backend: base.backend,
            rootfs_path: rootfs,
            machine_name: machine_name(id),
            state: EnvState::Created,
            profile: profile.name.clone(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            limits,
            network_policy: profile.network_policy.clone(),
            sessions: Vec::new(),
        };
        self.layout.write_env(&env).await?;
        Ok(env)
    }

    pub async fn env_start(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        env.state = EnvState::Running;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await
    }

    pub async fn env_stop(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        env.state = EnvState::Stopped;
        self.layout.write_env(&env).await
    }

    pub async fn env_destroy(&self, id: &str) -> Result<()> {
        let env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        remove_dir_all_if_exists(&self.layout.env_dir(id)).await
    }

    pub async fn env_status(&self, id: &str) -> Result<EnvStatus> {
        let env = self.layout.read_env(id).await?;
        let disk_used = dir_size(&env.rootfs_path).ok().map(human_bytes);
        Ok(EnvStatus { env, disk_used })
    }

    pub async fn exec(&self, id: &str, command: &[String]) -> Result<CmdOutput> {
        if command.is_empty() {
            return Err(anyhow!("exec command cannot be empty"));
        }
        let mut env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        if env.state != EnvState::Running {
            return Err(anyhow!("env {id} is not running"));
        }
        let (program, args) = command
            .split_first()
            .ok_or_else(|| anyhow!("exec command cannot be empty"))?;
        let output = self
            .runner
            .run_in_dir(&env.rootfs_path, program, args)
            .await?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(output)
    }
}

fn ensure_desktop_backend(env: &Env) -> Result<()> {
    if matches!(
        env.backend,
        RootfsBackend::ApfsClone | RootfsBackend::WindowsBlockClone
    ) {
        Ok(())
    } else {
        Err(anyhow!("env {} is not a desktop native env", env.id))
    }
}

async fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, DesktopService};
    use crate::config::AgentConfig;
    use crate::model::RootfsBackend;

    #[test]
    fn human_bytes_formats_compact_sizes() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(1536), "1.5K");
    }

    #[tokio::test]
    async fn rejects_non_desktop_base_backend() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        let error = service
            .base_freeze_with_backend("base-001", dir.path(), RootfsBackend::Overlay)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("not a desktop clone backend"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[tokio::test]
    async fn reports_unsupported_native_backend_on_linux() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        let error = service
            .base_freeze("base-001", dir.path())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("supported only on macOS and Windows"));
    }
}
