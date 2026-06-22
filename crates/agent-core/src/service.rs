use crate::btrfs::Btrfs;
use crate::command::{shell_join, CommandRunner};
use crate::config::AgentConfig;
use crate::export::{ExportType, Exporter};
use crate::model::{
    machine_name, Base, Env, EnvState, EnvStatus, LimitOverrides, RootfsBackend, Session,
    SessionState,
};
use crate::nspawn::Nspawn;
use crate::protocol::{Request, Response};
use crate::reflink;
use crate::session::TmuxSessionBackend;
use crate::storage::{validate_id, write_text_file, Layout};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AgentService {
    pub config: AgentConfig,
    layout: Layout,
    btrfs: Btrfs,
    nspawn: Nspawn,
    sessions: TmuxSessionBackend,
    exporter: Exporter,
    runner: CommandRunner,
}

impl AgentService {
    pub fn new(config: AgentConfig) -> Self {
        let layout = Layout::new(config.agentfs.clone());
        Self {
            config,
            layout,
            btrfs: Btrfs::default(),
            nspawn: Nspawn::default(),
            sessions: TmuxSessionBackend::default(),
            exporter: Exporter::default(),
            runner: CommandRunner,
        }
    }

    pub async fn handle(&self, request: Request) -> Response {
        match self.handle_result(request).await {
            Ok(response) => response,
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        }
    }

    async fn handle_result(&self, request: Request) -> Result<Response> {
        match request {
            Request::Init { agentfs } => {
                AgentService::new(AgentConfig::new(agentfs)).init().await?;
                Ok(Response::Ok)
            }
            Request::New {
                target,
                base,
                from,
                backend,
                profile,
                limits,
                command,
                cwd: _,
            } => {
                if backend.is_some() {
                    return Err(anyhow!(
                        "backend selection is supported only by the native desktop backend"
                    ));
                }
                self.new_target(&target, &base, &from, &profile, limits, &command)
                    .await
            }
            Request::BaseFreeze {
                name,
                from,
                backend,
            } => {
                if backend.is_some() {
                    return Err(anyhow!(
                        "backend selection is supported only by the native desktop backend"
                    ));
                }
                self.base_freeze(&name, &from).await?;
                Ok(Response::Ok)
            }
            Request::EnvCreate {
                id,
                base,
                profile,
                limits,
            } => {
                self.env_create(&id, &base, &profile, limits).await?;
                Ok(Response::Ok)
            }
            Request::EnvStart { id } => {
                self.env_start(&id).await?;
                Ok(Response::Ok)
            }
            Request::EnvStop { id } => {
                self.env_stop(&id).await?;
                Ok(Response::Ok)
            }
            Request::EnvDestroy { id } => {
                self.env_destroy(&id).await?;
                Ok(Response::Ok)
            }
            Request::EnvList => Ok(Response::Envs {
                envs: self.env_list().await?,
            }),
            Request::EnvStatus { id } => Ok(Response::EnvStatus {
                status: Box::new(self.env_status(&id).await?),
            }),
            Request::Exec { id, command, .. } => {
                let output = self.exec(&id, &command).await?;
                Ok(Response::Exec {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Request::Open { .. } => Err(anyhow!(
                "desktop app launching is supported only by the native desktop backend"
            )),
            Request::Shell { id, .. } => {
                let (machine_name, session_id) = self.shell_attach_target(&id).await?;
                Ok(Response::Attach {
                    machine_name,
                    session_id,
                })
            }
            Request::SessionCreate {
                env_id,
                session_id,
                command,
                ..
            } => {
                self.session_create(&env_id, &session_id, &command).await?;
                Ok(Response::Ok)
            }
            Request::SessionAttach { env_id, session_id } => {
                let env = self.session_attach_target(&env_id, &session_id).await?;
                Ok(Response::Attach {
                    machine_name: env.machine_name,
                    session_id,
                })
            }
            Request::SessionDetach { env_id, session_id } => {
                self.session_detach(&env_id, &session_id).await?;
                Ok(Response::Ok)
            }
            Request::SessionKill { env_id, session_id } => {
                self.session_kill(&env_id, &session_id).await?;
                Ok(Response::Ok)
            }
            Request::SessionList { env_id } => Ok(Response::Sessions {
                sessions: self.session_list(&env_id).await?,
            }),
            Request::SessionLogs { env_id, session_id } => Ok(Response::Text {
                text: self.session_logs(&env_id, &session_id).await?,
            }),
            Request::Diff { env_id } => Ok(Response::Text {
                text: self.diff(&env_id).await?,
            }),
            Request::Export {
                env_id,
                export_type,
            } => Ok(Response::Text {
                text: self
                    .export(&env_id, ExportType::parse(&export_type)?)
                    .await?,
            }),
            Request::Apply { .. } => Err(anyhow!(
                "apply is implemented only by the native desktop path-preserving overlay backend"
            )),
            Request::Ping => Ok(Response::Ok),
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.layout.ensure_agentfs().await?;
        if self.btrfs.is_filesystem(&self.config.agentfs).await? {
            self.btrfs.enable_quota(&self.config.agentfs).await?;
        }
        Ok(())
    }

    pub async fn new_target(
        &self,
        target: &str,
        base_id: &str,
        from: &Path,
        profile_name: &str,
        limit_overrides: LimitOverrides,
        command: &[String],
    ) -> Result<Response> {
        self.init().await?;
        self.ensure_base(base_id, from).await?;
        self.ensure_env(target, base_id, profile_name, limit_overrides)
            .await?;
        self.ensure_env_started(target).await?;
        if command.is_empty() {
            let (machine_name, session_id) = self.shell_attach_target(target).await?;
            Ok(Response::Attach {
                machine_name,
                session_id,
            })
        } else {
            let output = self.exec(target, command).await?;
            Ok(Response::Exec {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
    }

    pub async fn base_freeze(&self, name: &str, from: &Path) -> Result<Base> {
        validate_id(name)?;
        if self.btrfs.is_subvolume(from).await.unwrap_or(false) {
            return self.base_freeze_btrfs(name, from).await;
        }
        self.base_freeze_overlay(name, from).await
    }

    pub async fn base_freeze_native_clone(
        &self,
        name: &str,
        from: &Path,
        backend: RootfsBackend,
    ) -> Result<Base> {
        validate_id(name)?;
        if !matches!(
            backend,
            RootfsBackend::ApfsClone
                | RootfsBackend::WindowsBlockClone
                | RootfsBackend::WindowsMinifilterOverlay
        ) {
            return Err(anyhow!("backend {backend:?} is not a native clone backend"));
        }
        if !from.is_absolute() {
            return Err(anyhow!("native clone base source must be an absolute path"));
        }
        let base_dir = self.layout.base_dir(name);
        if base_dir.exists() {
            return Err(anyhow!("base {name} already exists"));
        }
        tokio::fs::create_dir_all(&base_dir).await?;
        let rootfs = self.layout.base_rootfs(name);
        if let Err(error) = reflink::clone_tree(from, &rootfs) {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        let dpkg_manifest = base_dir.join("dpkg.list");
        if let Err(error) = self.write_dpkg_manifest(&rootfs, &dpkg_manifest).await {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        let created_at = Utc::now();
        if let Err(error) =
            write_text_file(&base_dir.join("created_at"), created_at.to_rfc3339()).await
        {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        let base = Base {
            id: name.to_string(),
            backend,
            rootfs_path: rootfs,
            readonly: true,
            created_at,
            source: base_source_label(from),
            dpkg_manifest,
        };
        if let Err(error) = self.layout.write_base(&base).await {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        Ok(base)
    }

    async fn base_freeze_btrfs(&self, name: &str, from: &Path) -> Result<Base> {
        let base_dir = self.layout.base_dir(name);
        if base_dir.exists() {
            return Err(anyhow!("base {name} already exists"));
        }
        tokio::fs::create_dir_all(&base_dir).await?;
        let rootfs = self.layout.base_rootfs(name);
        if let Err(error) = self.btrfs.snapshot_writable(from, &rootfs).await {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        if let Err(error) = self.clean_runtime_paths(&rootfs).await {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        if let Err(error) = self.btrfs.set_readonly(&rootfs, true).await {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        let dpkg_manifest = base_dir.join("dpkg.list");
        if let Err(error) = self.write_dpkg_manifest(&rootfs, &dpkg_manifest).await {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        let created_at = Utc::now();
        if let Err(error) =
            write_text_file(&base_dir.join("created_at"), created_at.to_rfc3339()).await
        {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        let base = Base {
            id: name.to_string(),
            backend: RootfsBackend::Btrfs,
            rootfs_path: rootfs.clone(),
            readonly: true,
            created_at,
            source: base_source_label(from),
            dpkg_manifest,
        };
        if let Err(error) = self.layout.write_base(&base).await {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        Ok(base)
    }

    async fn base_freeze_overlay(&self, name: &str, from: &Path) -> Result<Base> {
        if !from.is_absolute() {
            return Err(anyhow!("overlay base source must be an absolute path"));
        }
        if !tokio::fs::try_exists(from).await? {
            return Err(anyhow!(
                "overlay base source {} does not exist",
                from.display()
            ));
        }
        let base_dir = self.layout.base_dir(name);
        if base_dir.exists() {
            return Err(anyhow!("base {name} already exists"));
        }
        tokio::fs::create_dir_all(&base_dir).await?;
        let dpkg_manifest = base_dir.join("dpkg.list");
        if let Err(error) = self.write_dpkg_manifest(from, &dpkg_manifest).await {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        let created_at = Utc::now();
        if let Err(error) =
            write_text_file(&base_dir.join("created_at"), created_at.to_rfc3339()).await
        {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
        let base = Base {
            id: name.to_string(),
            backend: RootfsBackend::Overlay,
            rootfs_path: from.to_path_buf(),
            readonly: true,
            created_at,
            source: base_source_label(from),
            dpkg_manifest,
        };
        if let Err(error) = self.layout.write_base(&base).await {
            cleanup_failed_base_dir(&base_dir).await;
            return Err(error);
        }
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
        let env_dir = self.layout.env_dir(id);
        if env_dir.exists() {
            return Err(anyhow!("env {id} already exists"));
        }
        let limits = profile.limits.clone().with_overrides(limit_overrides);
        limits.validate()?;
        let rootfs = self.layout.env_rootfs(id);
        tokio::fs::create_dir_all(&env_dir).await?;
        match base.backend {
            RootfsBackend::Btrfs => {
                if let Err(error) = self
                    .btrfs
                    .snapshot_writable(&base.rootfs_path, &rootfs)
                    .await
                {
                    cleanup_failed_env_dir(&env_dir).await;
                    return Err(error);
                }
                if let Err(error) = self.btrfs.set_limit(&limits.disk_max, &rootfs).await {
                    self.cleanup_failed_env_create(RootfsBackend::Btrfs, &rootfs, &env_dir)
                        .await;
                    return Err(error);
                }
            }
            RootfsBackend::Overlay => {
                if let Err(error) = self.ensure_overlay_dirs(id).await {
                    cleanup_failed_env_dir(&env_dir).await;
                    return Err(error);
                }
            }
            RootfsBackend::ApfsClone
            | RootfsBackend::WindowsBlockClone
            | RootfsBackend::PathPreservingOverlay
            | RootfsBackend::WindowsMinifilterOverlay => {
                if let Err(error) = reflink::clone_tree(&base.rootfs_path, &rootfs) {
                    cleanup_failed_env_dir(&env_dir).await;
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.ensure_env_dirs(id).await {
            self.cleanup_failed_env_create(base.backend.clone(), &rootfs, &env_dir)
                .await;
            return Err(error);
        }
        let env = Env {
            id: id.to_string(),
            base_id: base_id.to_string(),
            backend: base.backend.clone(),
            rootfs_path: rootfs.clone(),
            machine_name: machine_name(id),
            state: EnvState::Created,
            profile: profile.name.clone(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            limits,
            network_policy: profile.network_policy.clone(),
            sessions: Vec::new(),
        };
        if let Err(error) = self.log_daemon(id, "env created").await {
            self.cleanup_failed_env_create(env.backend.clone(), &rootfs, &env_dir)
                .await;
            return Err(error);
        }
        if let Err(error) = self.log_lifecycle(id, "created").await {
            self.cleanup_failed_env_create(env.backend.clone(), &rootfs, &env_dir)
                .await;
            return Err(error);
        }
        if let Err(error) = self.nspawn.write_config(&env).await {
            self.cleanup_failed_env_create(env.backend.clone(), &rootfs, &env_dir)
                .await;
            return Err(error);
        }
        if let Err(error) = self.layout.write_env(&env).await {
            let _ = self.nspawn.remove_config(&env).await;
            self.cleanup_failed_env_create(env.backend.clone(), &rootfs, &env_dir)
                .await;
            return Err(error);
        }
        Ok(env)
    }

    pub async fn env_start(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        let nspawn_log = self.layout.nspawn_log(id);
        self.log_daemon(id, "env start requested").await?;
        self.log_lifecycle(id, "starting").await?;
        if let Err(error) = self.ensure_env_rootfs_mounted(&env).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(id, &format!("env start failed mount setup: {error:#}"))
                .await?;
            self.log_lifecycle(id, &format!("failed mount setup: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = validate_child_rootfs_requirements(&env.rootfs_path) {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(id, &format!("env start failed preflight: {error:#}"))
                .await?;
            self.log_lifecycle(id, &format!("failed preflight: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = ensure_inaccessible_mask_targets(&env.rootfs_path).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(
                id,
                &format!("env start failed mask target setup: {error:#}"),
            )
            .await?;
            self.log_lifecycle(id, &format!("failed mask target setup: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = ensure_child_hostname(&env.rootfs_path, &env.machine_name).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(id, &format!("env start failed hostname setup: {error:#}"))
                .await?;
            self.log_lifecycle(id, &format!("failed hostname setup: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = ensure_child_network_config(&env).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(id, &format!("env start failed network setup: {error:#}"))
                .await?;
            self.log_lifecycle(id, &format!("failed network setup: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = self.nspawn.start(&env, Some(&nspawn_log)).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_daemon(id, &format!("env start failed: {error:#}"))
                .await?;
            self.log_lifecycle(id, &format!("failed start: {error:#}"))
                .await?;
            return Err(error);
        }
        env.state = EnvState::Running;
        mark_env_active(&mut env);
        self.log_daemon(id, "env started").await?;
        self.log_lifecycle(id, "running").await?;
        self.layout.write_env(&env).await?;
        Ok(())
    }

    async fn ensure_base(&self, base_id: &str, from: &Path) -> Result<Base> {
        validate_id(base_id)?;
        let manifest = self.layout.base_dir(base_id).join("manifest.json");
        if tokio::fs::try_exists(&manifest).await? {
            return self.layout.read_base(base_id).await;
        }
        self.base_freeze(base_id, from).await
    }

    async fn ensure_env(
        &self,
        id: &str,
        base_id: &str,
        profile_name: &str,
        limit_overrides: LimitOverrides,
    ) -> Result<Env> {
        validate_id(id)?;
        let metadata = self.layout.env_dir(id).join("meta.json");
        if tokio::fs::try_exists(&metadata).await? {
            return self.layout.read_env(id).await;
        }
        self.env_create(id, base_id, profile_name, limit_overrides)
            .await
    }

    async fn ensure_env_started(&self, id: &str) -> Result<()> {
        let status = self.env_status(id).await?;
        match status.env.state {
            EnvState::Running => Ok(()),
            EnvState::Created | EnvState::Stopped => self.env_start(id).await,
            EnvState::Failed | EnvState::QuotaExceeded => Err(anyhow!(
                "env {id} is {:?}; fix or destroy it before running new",
                status.env.state
            )),
        }
    }

    pub async fn env_stop(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        self.log_daemon(id, "env stop requested").await?;
        self.log_lifecycle(id, "stopping").await?;
        self.nspawn.stop(&env).await?;
        let mut refreshed = env.clone();
        self.nspawn.refresh_state(&mut refreshed).await?;
        if refreshed.state == EnvState::Running {
            self.log_lifecycle(id, "stop requested but machine is still running")
                .await?;
            return Err(anyhow!("env {id} is still running after stop"));
        }
        apply_stopped_state(&mut env);
        if env.backend == RootfsBackend::Overlay {
            self.umount_overlay_rootfs(&env).await?;
        }
        self.log_daemon(id, "env stopped").await?;
        self.log_lifecycle(id, "stopped").await?;
        self.layout.write_env(&env).await?;
        Ok(())
    }

    pub async fn env_destroy(&self, id: &str) -> Result<()> {
        let env = self.layout.read_env(id).await?;
        self.log_daemon(id, "env destroy requested").await?;
        self.log_lifecycle(id, "destroying").await?;
        self.nspawn.stop(&env).await?;
        match env.backend {
            RootfsBackend::Btrfs => {
                let qgroup_id = self.btrfs.qgroup_id(&env.rootfs_path).await?;
                self.btrfs.delete_subvolume(&env.rootfs_path).await?;
                if let Some(qgroup_id) = qgroup_id {
                    self.btrfs
                        .destroy_qgroup(&qgroup_id, &self.config.agentfs)
                        .await?;
                }
            }
            RootfsBackend::Overlay => {
                self.umount_overlay_rootfs(&env).await?;
            }
            RootfsBackend::ApfsClone
            | RootfsBackend::WindowsBlockClone
            | RootfsBackend::PathPreservingOverlay
            | RootfsBackend::WindowsMinifilterOverlay => {
                remove_dir_all_if_exists(&env.rootfs_path).await?;
            }
        }
        self.nspawn.remove_config(&env).await?;
        remove_dir_all_if_exists(&self.layout.env_dir(id)).await?;
        Ok(())
    }

    pub async fn env_status(&self, id: &str) -> Result<EnvStatus> {
        let mut env = self.layout.read_env(id).await?;
        if should_refresh_live_state(&env.state) {
            self.nspawn.refresh_state(&mut env).await?;
        }
        if idle_timeout_expired(&env, Utc::now()) {
            self.nspawn.stop(&env).await?;
            apply_stopped_state(&mut env);
            self.log_lifecycle(id, "idle timeout exceeded").await?;
        }
        if env.backend == RootfsBackend::Btrfs
            && should_check_quota(&env.state)
            && self.btrfs.quota_exceeded(&env.rootfs_path).await?
        {
            env.state = EnvState::QuotaExceeded;
            self.log_lifecycle(id, "quota exceeded during status refresh")
                .await?;
        }
        self.layout.write_env(&env).await?;
        let disk_used = self.env_disk_used(&env).await.ok();
        Ok(EnvStatus { env, disk_used })
    }

    pub async fn env_list(&self) -> Result<Vec<EnvStatus>> {
        let envs = self.layout.list_envs().await?;
        let mut statuses = Vec::with_capacity(envs.len());
        for env in envs {
            statuses.push(self.env_status(&env.id).await?);
        }
        Ok(statuses)
    }

    pub async fn exec(&self, id: &str, command: &[String]) -> Result<crate::command::CmdOutput> {
        if command.is_empty() {
            return Err(anyhow!("exec command cannot be empty"));
        }
        let mut env = self.layout.read_env(id).await?;
        ensure_running_env(&env)?;
        let log_path = self.layout.env_logs(id).join("exec.log");
        self.log_daemon(id, &format!("exec {}", shell_join(command)))
            .await?;
        self.log_lifecycle(id, &format!("exec {}", shell_join(command)))
            .await?;
        let output = self.nspawn.exec(&env, command, &log_path).await?;
        mark_env_active(&mut env);
        if env.backend == RootfsBackend::Btrfs
            && self.btrfs.quota_exceeded(&env.rootfs_path).await?
        {
            env.state = EnvState::QuotaExceeded;
            self.log_lifecycle(id, "quota exceeded after exec").await?;
        }
        self.layout.write_env(&env).await?;
        Ok(output)
    }

    pub async fn session_create(
        &self,
        env_id: &str,
        session_id: &str,
        command: &[String],
    ) -> Result<()> {
        validate_id(session_id)?;
        if command.is_empty() {
            return Err(anyhow!("session command cannot be empty"));
        }
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let metadata_path = self
            .layout
            .sessions_dir(env_id)
            .join(format!("{session_id}.json"));
        let metadata_exists = tokio::fs::try_exists(&metadata_path).await?;
        let running = match self.layout.read_session(env_id, session_id).await {
            Ok(_) => self.sessions.is_running(&env, session_id).await?,
            Err(_) if !metadata_exists => false,
            Err(error) => {
                return Err(error.context(format!(
                    "session {session_id} metadata exists but could not be read"
                )));
            }
        };
        if should_reject_session_create(metadata_exists, running) {
            return Err(anyhow!(
                "session {session_id} is already running in env {env_id}"
            ));
        }
        let log_path = TmuxSessionBackend::log_path(&self.layout.session_logs(env_id), session_id);
        let session = self
            .sessions
            .create(&env, session_id, command, log_path)
            .await?;
        if !env.sessions.iter().any(|existing| existing == session_id) {
            env.sessions.push(session_id.to_string());
            env.sessions.sort();
        }
        mark_env_active(&mut env);
        self.layout.write_session(&session).await?;
        self.layout.write_env(&env).await?;
        self.log_daemon(env_id, &format!("session {session_id} created"))
            .await?;
        self.log_lifecycle(env_id, &format!("session {session_id} created"))
            .await?;
        Ok(())
    }

    pub async fn shell_attach_target(&self, env_id: &str) -> Result<(String, String)> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let session_id = "shell";
        let metadata_path = self
            .layout
            .sessions_dir(env_id)
            .join(format!("{session_id}.json"));
        let metadata_exists = tokio::fs::try_exists(&metadata_path).await?;
        let running = match self.layout.read_session(env_id, session_id).await {
            Ok(_) => self.sessions.is_running(&env, session_id).await?,
            Err(_) if !metadata_exists => false,
            Err(error) => {
                return Err(error.context("shell session metadata exists but could not be read"));
            }
        };
        if !running {
            self.session_create(env_id, session_id, &default_shell_command())
                .await?;
            env = self.layout.read_env(env_id).await?;
        }
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        Ok((env.machine_name, session_id.to_string()))
    }

    pub async fn session_attach_target(&self, env_id: &str, session_id: &str) -> Result<Env> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        if !self.sessions.is_running(&env, session_id).await? {
            session.state = SessionState::Stopped;
            self.layout.write_session(&session).await?;
            return Err(anyhow!(
                "session {session_id} in env {env_id} is not running"
            ));
        }
        if session.state != SessionState::Running {
            session.state = SessionState::Running;
            self.layout.write_session(&session).await?;
        }
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        Ok(env)
    }

    pub async fn session_logs(&self, env_id: &str, session_id: &str) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        let session = self.layout.read_session(env_id, session_id).await?;
        let logs = if env.state == EnvState::Running {
            self.sessions
                .logs(&env, session_id, &session.log_path)
                .await?
        } else {
            read_offline_session_log(
                &TmuxSessionBackend::host_transcript_path(&env, session_id),
                &session.log_path,
            )
            .await?
        };
        self.log_daemon(env_id, &format!("session {session_id} logs synced"))
            .await?;
        self.log_lifecycle(env_id, &format!("session {session_id} logs synced"))
            .await?;
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        Ok(logs)
    }

    pub async fn session_detach(&self, env_id: &str, session_id: &str) -> Result<()> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        if !self.sessions.is_running(&env, &session.id).await? {
            session.state = SessionState::Stopped;
            self.layout.write_session(&session).await?;
            return Err(anyhow!(
                "session {session_id} in env {env_id} is not running"
            ));
        }
        self.sessions.detach(&env, &session.id).await?;
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        self.log_daemon(env_id, &format!("session {session_id} detached"))
            .await?;
        self.log_lifecycle(env_id, &format!("session {session_id} detached"))
            .await?;
        Ok(())
    }

    pub async fn session_kill(&self, env_id: &str, session_id: &str) -> Result<()> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        self.sessions.kill(&env, &session.id).await?;
        session.state = SessionState::Stopped;
        self.layout.write_session(&session).await?;
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        self.log_daemon(env_id, &format!("session {session_id} killed"))
            .await?;
        self.log_lifecycle(env_id, &format!("session {session_id} killed"))
            .await?;
        Ok(())
    }

    pub async fn session_list(&self, env_id: &str) -> Result<Vec<crate::model::Session>> {
        let mut env = self.layout.read_env(env_id).await?;
        let live_sessions = if env.state == EnvState::Running {
            self.sessions.list(&env).await?
        } else {
            Vec::new()
        };
        let mut sessions = self.layout.list_sessions(env_id).await?;
        for session in &mut sessions {
            session.state = if live_sessions.iter().any(|live| live == &session.id) {
                crate::model::SessionState::Running
            } else {
                crate::model::SessionState::Stopped
            };
            self.layout.write_session(session).await?;
        }
        if sync_env_session_index(&mut env, &sessions) {
            self.layout.write_env(&env).await?;
        }
        Ok(sessions)
    }

    pub async fn diff(&self, env_id: &str) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        self.ensure_env_rootfs_mounted(&env).await?;
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        self.exporter.workspace_patch(&env).await
    }

    pub async fn export(&self, env_id: &str, export_type: ExportType) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        let base = self.layout.read_base(&env.base_id).await?;
        self.ensure_env_rootfs_mounted(&env).await?;
        let text = match export_type {
            ExportType::WorkspacePatch => self.exporter.workspace_patch(&env).await,
            ExportType::RootfsChangedPaths => match env.backend {
                RootfsBackend::Btrfs => {
                    Exporter::changed_paths_by_walk(&base.rootfs_path, &env.rootfs_path)
                }
                RootfsBackend::Overlay => self.overlay_changed_paths(&env).await,
                RootfsBackend::ApfsClone
                | RootfsBackend::WindowsBlockClone
                | RootfsBackend::PathPreservingOverlay
                | RootfsBackend::WindowsMinifilterOverlay => {
                    Exporter::changed_paths_by_walk(&base.rootfs_path, &env.rootfs_path)
                }
            },
            ExportType::DpkgDelta => {
                let env_manifest = self.layout.env_dir(env_id).join("dpkg.list");
                self.write_dpkg_manifest(&env.rootfs_path, &env_manifest)
                    .await?;
                Exporter::dpkg_delta(&base.dpkg_manifest, &env_manifest)
            }
        }?;
        let artifact = self
            .layout
            .env_dir(env_id)
            .join("exports")
            .join(export_type.artifact_name());
        write_text_file(&artifact, &text).await?;
        mark_env_active(&mut env);
        self.layout.write_env(&env).await?;
        self.log_daemon(env_id, &format!("exported {}", artifact.display()))
            .await?;
        self.log_lifecycle(env_id, &format!("exported {}", artifact.display()))
            .await?;
        Ok(text)
    }

    async fn write_dpkg_manifest(&self, rootfs: &Path, target: &Path) -> Result<()> {
        let status = rootfs.join("var/lib/dpkg/status");
        if status.exists() {
            let text = tokio::fs::read_to_string(status).await?;
            let mut packages = Vec::new();
            for block in text.split("\n\n") {
                let name = block
                    .lines()
                    .find_map(|line| line.strip_prefix("Package: "))
                    .unwrap_or_default();
                let state = block
                    .lines()
                    .find_map(|line| line.strip_prefix("Status: "))
                    .unwrap_or_default();
                let version = block
                    .lines()
                    .find_map(|line| line.strip_prefix("Version: "))
                    .unwrap_or("unknown");
                if !name.is_empty() && state.contains(" installed") {
                    packages.push(format!("{name} {version}"));
                }
            }
            packages.sort();
            write_text_file(target, format!("{}\n", packages.join("\n"))).await?;
            return Ok(());
        }

        let output = self
            .runner
            .run(
                "chroot",
                vec![
                    rootfs.display().to_string(),
                    "dpkg-query".to_string(),
                    "-W".to_string(),
                    "-f=${Package} ${Version}\\n".to_string(),
                ],
            )
            .await
            .context("failed to collect dpkg manifest")?;
        if output.status != 0 {
            return Err(anyhow!("dpkg-query failed: {}", output.stderr));
        }
        write_text_file(target, output.stdout).await?;
        Ok(())
    }

    async fn ensure_overlay_dirs(&self, id: &str) -> Result<()> {
        for dir in [
            self.overlay_upper_dir(id),
            self.overlay_work_dir(id),
            self.layout.env_rootfs(id),
        ] {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    async fn ensure_env_rootfs_mounted(&self, env: &Env) -> Result<()> {
        if env.backend != RootfsBackend::Overlay {
            return Ok(());
        }
        self.ensure_overlay_dirs(&env.id).await?;
        if self.is_mountpoint(&env.rootfs_path).await? {
            return Ok(());
        }
        let base = self.layout.read_base(&env.base_id).await?;
        let lower = base.rootfs_path;
        let upper = self.overlay_upper_dir(&env.id);
        let work = self.overlay_work_dir(&env.id);
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        self.runner
            .run_checked(
                "mount",
                [
                    "-t",
                    "overlay",
                    "overlay",
                    "-o",
                    &options,
                    &env.rootfs_path.display().to_string(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn umount_overlay_rootfs(&self, env: &Env) -> Result<()> {
        if env.backend != RootfsBackend::Overlay || !self.is_mountpoint(&env.rootfs_path).await? {
            return Ok(());
        }
        self.runner
            .run_checked("umount", [&env.rootfs_path.display().to_string()])
            .await?;
        Ok(())
    }

    async fn is_mountpoint(&self, path: &Path) -> Result<bool> {
        let output = self
            .runner
            .run(
                "findmnt",
                ["-n", "--mountpoint", &path.display().to_string()],
            )
            .await?;
        Ok(output.status == 0)
    }

    async fn env_disk_used(&self, env: &Env) -> Result<String> {
        let path = match env.backend {
            RootfsBackend::Btrfs
            | RootfsBackend::ApfsClone
            | RootfsBackend::WindowsBlockClone
            | RootfsBackend::PathPreservingOverlay
            | RootfsBackend::WindowsMinifilterOverlay => env.rootfs_path.clone(),
            RootfsBackend::Overlay => self.overlay_upper_dir(&env.id),
        };
        self.disk_used(&path).await
    }

    async fn overlay_changed_paths(&self, env: &Env) -> Result<String> {
        let upper = self.overlay_upper_dir(&env.id);
        let mut paths = Vec::new();
        collect_overlay_changed_paths(&upper, &upper, &mut paths)?;
        paths.sort();
        Ok(format!("{}\n", paths.join("\n")).trim_end().to_string())
    }

    fn overlay_upper_dir(&self, id: &str) -> PathBuf {
        self.layout.env_dir(id).join("upper")
    }

    fn overlay_work_dir(&self, id: &str) -> PathBuf {
        self.layout.env_dir(id).join("work")
    }

    async fn disk_used(&self, path: &Path) -> Result<String> {
        let output = self
            .runner
            .run_checked("du", ["-sh", &path.display().to_string()])
            .await?;
        Ok(output
            .stdout
            .split_whitespace()
            .next()
            .unwrap_or("-")
            .to_string())
    }

    async fn clean_runtime_paths(&self, rootfs: &Path) -> Result<()> {
        for rel in ["proc", "sys", "dev", "run", "tmp", "agentfs"] {
            let path = rootfs.join(rel);
            remove_path_if_exists(&path).await?;
            if matches!(rel, "proc" | "sys" | "dev" | "run" | "tmp") {
                tokio::fs::create_dir_all(&path).await?;
                if rel == "tmp" {
                    tokio::fs::set_permissions(&path, Permissions::from_mode(0o1777)).await?;
                }
            }
        }
        tokio::fs::create_dir_all(rootfs.join("agentfs")).await?;
        Ok(())
    }

    async fn log_daemon(&self, env_id: &str, message: &str) -> Result<()> {
        self.append_env_log(&self.layout.daemon_log(env_id), message)
            .await
    }

    async fn log_lifecycle(&self, env_id: &str, message: &str) -> Result<()> {
        self.append_env_log(&self.layout.lifecycle_log(env_id), message)
            .await
    }

    async fn append_env_log(&self, path: &Path, message: &str) -> Result<()> {
        let line = format!("{} {message}\n", Utc::now().to_rfc3339());
        CommandRunner::append_to_file(path, &line).await
    }

    async fn ensure_env_dirs(&self, id: &str) -> Result<()> {
        let env_dir = self.layout.env_dir(id);
        tokio::fs::create_dir_all(self.layout.session_logs(id)).await?;
        tokio::fs::create_dir_all(self.layout.sessions_dir(id)).await?;
        tokio::fs::create_dir_all(env_dir.join("exports")).await?;
        tokio::fs::create_dir_all(env_dir.join("locks")).await?;
        Ok(())
    }

    async fn cleanup_failed_env_create(
        &self,
        backend: RootfsBackend,
        rootfs: &Path,
        env_dir: &Path,
    ) {
        match backend {
            RootfsBackend::Btrfs => {
                let qgroup_id = self.btrfs.qgroup_id(rootfs).await.ok().flatten();
                let _ = self.btrfs.delete_subvolume(rootfs).await;
                if let Some(qgroup_id) = qgroup_id {
                    let _ = self
                        .btrfs
                        .destroy_qgroup(&qgroup_id, &self.config.agentfs)
                        .await;
                }
            }
            RootfsBackend::ApfsClone
            | RootfsBackend::WindowsBlockClone
            | RootfsBackend::PathPreservingOverlay
            | RootfsBackend::WindowsMinifilterOverlay => {
                let _ = remove_dir_all_if_exists(rootfs).await;
            }
            RootfsBackend::Overlay => {}
        }
        cleanup_failed_env_dir(env_dir).await;
    }

    async fn cleanup_failed_base_freeze(&self, rootfs: &Path, base_dir: &Path) {
        let _ = self.btrfs.set_readonly(rootfs, false).await;
        let _ = self.btrfs.delete_subvolume(rootfs).await;
        cleanup_failed_base_dir(base_dir).await;
    }
}

async fn cleanup_failed_base_dir(base_dir: &Path) {
    let _ = remove_dir_all_if_exists(base_dir).await;
}

async fn cleanup_failed_env_dir(env_dir: &Path) {
    let _ = remove_dir_all_if_exists(env_dir).await;
}

async fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        tokio::fs::remove_dir_all(path).await?;
    } else {
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

fn collect_overlay_changed_paths(root: &Path, dir: &Path, paths: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name == ".wh..wh..opq" {
            continue;
        }
        let reported_path = if let Some(deleted_name) = file_name.strip_prefix(".wh.") {
            path.with_file_name(deleted_name)
        } else {
            path.clone()
        };
        let relative = reported_path
            .strip_prefix(root)
            .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
        paths.push(format!("/{}", relative.display()));
        if path.is_dir() {
            collect_overlay_changed_paths(root, &path, paths)?;
        }
    }
    Ok(())
}

fn should_refresh_live_state(state: &EnvState) -> bool {
    matches!(
        state,
        EnvState::Created | EnvState::Running | EnvState::Stopped
    )
}

fn should_check_quota(state: &EnvState) -> bool {
    !matches!(state, EnvState::Failed | EnvState::QuotaExceeded)
}

fn should_mark_stopped(state: &EnvState) -> bool {
    matches!(
        state,
        EnvState::Created | EnvState::Running | EnvState::Stopped
    )
}

fn apply_stopped_state(env: &mut Env) {
    if should_mark_stopped(&env.state) {
        env.state = EnvState::Stopped;
    }
}

fn mark_env_active(env: &mut Env) {
    env.last_active_at = Utc::now();
}

fn idle_timeout_expired(env: &Env, now: chrono::DateTime<Utc>) -> bool {
    if env.state != EnvState::Running {
        return false;
    }
    let Ok(Some(timeout)) = env.limits.idle_timeout_duration() else {
        return false;
    };
    now.signed_duration_since(env.last_active_at) >= timeout
}

fn ensure_running_env(env: &Env) -> Result<()> {
    if env.state == EnvState::Running {
        Ok(())
    } else {
        Err(anyhow!(
            "env {} is {:?}; start it before running commands or sessions",
            env.id,
            env.state
        ))
    }
}

fn should_reject_session_create(metadata_exists: bool, running: bool) -> bool {
    metadata_exists && running
}

fn sync_env_session_index(env: &mut Env, sessions: &[Session]) -> bool {
    let mut session_ids = sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    session_ids.sort();
    session_ids.dedup();
    if env.sessions == session_ids {
        false
    } else {
        env.sessions = session_ids;
        true
    }
}

fn default_shell_command() -> Vec<String> {
    vec!["/bin/bash".to_string()]
}

fn base_source_label(from: &Path) -> String {
    if from == Path::new("/") {
        "current-project-vm".to_string()
    } else {
        from.display().to_string()
    }
}

async fn read_session_log_file(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

async fn read_offline_session_log(child_transcript: &Path, agentfs_log: &Path) -> Result<String> {
    match tokio::fs::read_to_string(child_transcript).await {
        Ok(text) => {
            if let Some(parent) = agentfs_log.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            write_text_file(agentfs_log, &text).await?;
            Ok(text)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            read_session_log_file(agentfs_log).await
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_child_rootfs_requirements(rootfs: &Path) -> Result<()> {
    let mut missing = Vec::new();
    for (name, candidates) in [
        ("bash", &["bin/bash"][..]),
        ("sudo", &["usr/bin/sudo", "bin/sudo"][..]),
        ("tmux", &["usr/bin/tmux", "bin/tmux"][..]),
        ("tee", &["usr/bin/tee", "bin/tee"][..]),
        ("apt", &["usr/bin/apt", "usr/bin/apt-get"][..]),
    ] {
        if !candidates
            .iter()
            .any(|candidate| rootfs_executable_exists(&rootfs.join(candidate)))
        {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "child rootfs {} is missing required tool(s): {}",
            rootfs.display(),
            missing.join(", ")
        ))
    }
}

fn rootfs_executable_exists(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

async fn ensure_inaccessible_mask_targets(rootfs: &Path) -> Result<()> {
    for path in Nspawn::inaccessible_paths() {
        let relative = path
            .strip_prefix('/')
            .ok_or_else(|| anyhow!("inaccessible path {path} must be absolute"))?;
        let target = rootfs.join(relative);
        if target.exists() {
            continue;
        }
        if *path == "/agentfs" {
            tokio::fs::create_dir_all(&target).await?;
            continue;
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .await?;
    }
    Ok(())
}

async fn ensure_child_hostname(rootfs: &Path, hostname: &str) -> Result<()> {
    write_text_file(&rootfs.join("etc/hostname"), &format!("{hostname}\n")).await
}

async fn ensure_child_network_config(env: &Env) -> Result<()> {
    if !env.limits.network_uses_bridge() {
        return Ok(());
    }
    let network_path = env
        .rootfs_path
        .join("etc/systemd/network/80-agent-forkd-host0.network");
    let address = format!("10.77.0.{}/24", child_ipv4_octet(&env.id));
    write_text_file(
        &network_path,
        &format!(
            "[Match]\nName=host0\n\n[Network]\nAddress={address}\nGateway=10.77.0.1\nDNS=1.1.1.1\nDNS=8.8.8.8\nIPv6AcceptRA=no\n"
        ),
    )
    .await?;
    enable_child_systemd_unit(&env.rootfs_path, "systemd-networkd.service").await?;
    enable_child_systemd_unit(&env.rootfs_path, "systemd-resolved.service").await?;
    Ok(())
}

fn child_ipv4_octet(env_id: &str) -> u8 {
    let hash = env_id
        .bytes()
        .fold(0u8, |hash, byte| hash.wrapping_mul(31).wrapping_add(byte));
    2 + (hash % 253)
}

async fn enable_child_systemd_unit(rootfs: &Path, unit: &str) -> Result<()> {
    let wants = rootfs.join("etc/systemd/system/multi-user.target.wants");
    tokio::fs::create_dir_all(&wants).await?;
    let link = wants.join(unit);
    if tokio::fs::symlink_metadata(&link).await.is_ok() {
        return Ok(());
    }
    let target = format!("/usr/lib/systemd/system/{unit}");
    std::os::unix::fs::symlink(&target, &link)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_stopped_state, base_source_label, child_ipv4_octet, cleanup_failed_base_dir,
        cleanup_failed_env_dir, collect_overlay_changed_paths, default_shell_command,
        ensure_child_hostname, ensure_child_network_config, ensure_inaccessible_mask_targets,
        ensure_running_env, idle_timeout_expired, read_offline_session_log, read_session_log_file,
        remove_dir_all_if_exists, remove_path_if_exists, should_check_quota, should_mark_stopped,
        should_refresh_live_state, should_reject_session_create, sync_env_session_index,
        validate_child_rootfs_requirements, AgentService,
    };
    use crate::config::AgentConfig;
    use crate::model::{
        machine_name, Env, EnvState, Limits, RootfsBackend, Session, SessionState, SessionType,
    };
    use chrono::Utc;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn rootfs_preflight_requires_sudo_apt_tmux_tee_and_bash() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("usr/bin")).unwrap();
        write_executable(&dir.path().join("bin/bash"));
        write_executable(&dir.path().join("usr/bin/sudo"));
        write_executable(&dir.path().join("usr/bin/apt-get"));
        write_executable(&dir.path().join("usr/bin/tmux"));
        write_executable(&dir.path().join("usr/bin/tee"));
        validate_child_rootfs_requirements(dir.path()).unwrap();
    }

    #[test]
    fn rootfs_preflight_reports_missing_tools() {
        let dir = tempfile::tempdir().unwrap();
        let error = validate_child_rootfs_requirements(dir.path()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("bash"));
        assert!(message.contains("sudo"));
        assert!(message.contains("apt"));
        assert!(message.contains("tmux"));
        assert!(message.contains("tee"));
    }

    #[test]
    fn rootfs_preflight_requires_bin_bash_for_machinectl_shell() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("usr/bin")).unwrap();
        write_executable(&dir.path().join("usr/bin/bash"));
        write_executable(&dir.path().join("usr/bin/sudo"));
        write_executable(&dir.path().join("usr/bin/apt"));
        write_executable(&dir.path().join("usr/bin/tmux"));
        write_executable(&dir.path().join("usr/bin/tee"));

        let error = validate_child_rootfs_requirements(dir.path()).unwrap_err();

        assert!(error.to_string().contains("bash"));
    }

    #[test]
    fn rootfs_preflight_rejects_non_executable_tools() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("usr/bin")).unwrap();
        fs::write(dir.path().join("bin/bash"), "").unwrap();
        write_executable(&dir.path().join("usr/bin/sudo"));
        write_executable(&dir.path().join("usr/bin/apt"));
        write_executable(&dir.path().join("usr/bin/tmux"));
        write_executable(&dir.path().join("usr/bin/tee"));

        let error = validate_child_rootfs_requirements(dir.path()).unwrap_err();

        assert!(error.to_string().contains("bash"));
    }

    #[tokio::test]
    async fn inaccessible_mask_targets_are_created_inside_child_rootfs() {
        let dir = tempfile::tempdir().unwrap();

        ensure_inaccessible_mask_targets(dir.path()).await.unwrap();

        assert!(dir.path().join("agentfs").is_dir());
        assert!(!dir.path().join("run/agent-forkd.sock").exists());
        assert!(!dir.path().join("run/docker.sock").exists());
        assert!(!dir.path().join("var/run/docker.sock").exists());
    }

    #[tokio::test]
    async fn child_hostname_is_written_inside_child_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("etc")).unwrap();

        ensure_child_hostname(dir.path(), "af-codex-1")
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("etc/hostname")).unwrap(),
            "af-codex-1\n"
        );
    }

    #[tokio::test]
    async fn bridge_network_enables_child_networkd_for_static_host0_networking() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = test_env(EnvState::Created);
        env.rootfs_path = dir.path().to_path_buf();
        env.limits.network = "bridge".to_string();

        ensure_child_network_config(&env).await.unwrap();

        assert_eq!(
            fs::read_to_string(
                dir.path()
                    .join("etc/systemd/network/80-agent-forkd-host0.network")
            )
            .unwrap(),
            format!(
                "[Match]\nName=host0\n\n[Network]\nAddress=10.77.0.{}/24\nGateway=10.77.0.1\nDNS=1.1.1.1\nDNS=8.8.8.8\nIPv6AcceptRA=no\n",
                child_ipv4_octet(&env.id)
            )
        );
        assert!(fs::symlink_metadata(
            dir.path()
                .join("etc/systemd/system/multi-user.target.wants/systemd-networkd.service")
        )
        .is_ok());
        assert!(fs::symlink_metadata(
            dir.path()
                .join("etc/systemd/system/multi-user.target.wants/systemd-resolved.service")
        )
        .is_ok());
    }

    #[tokio::test]
    async fn host_network_skips_child_networkd_setup() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = test_env(EnvState::Created);
        env.rootfs_path = dir.path().to_path_buf();

        ensure_child_network_config(&env).await.unwrap();

        assert!(!dir
            .path()
            .join("etc/systemd/network/80-agent-forkd-host0.network")
            .exists());
    }

    #[test]
    fn child_ipv4_octet_stays_in_bridge_host_range() {
        for id in ["codex-1", "claude-1", "idle-1", "runtime-1"] {
            assert!((2..=254).contains(&child_ipv4_octet(id)));
        }
    }

    #[test]
    fn terminal_env_states_are_not_overwritten_by_status_refresh() {
        assert!(should_refresh_live_state(&EnvState::Created));
        assert!(should_refresh_live_state(&EnvState::Running));
        assert!(should_refresh_live_state(&EnvState::Stopped));
        assert!(!should_refresh_live_state(&EnvState::Failed));
        assert!(!should_refresh_live_state(&EnvState::QuotaExceeded));
    }

    #[test]
    fn terminal_env_states_skip_quota_rechecks() {
        assert!(should_check_quota(&EnvState::Created));
        assert!(should_check_quota(&EnvState::Running));
        assert!(should_check_quota(&EnvState::Stopped));
        assert!(!should_check_quota(&EnvState::Failed));
        assert!(!should_check_quota(&EnvState::QuotaExceeded));
    }

    #[test]
    fn stop_preserves_terminal_env_states() {
        assert!(should_mark_stopped(&EnvState::Created));
        assert!(should_mark_stopped(&EnvState::Running));
        assert!(should_mark_stopped(&EnvState::Stopped));
        assert!(!should_mark_stopped(&EnvState::Failed));
        assert!(!should_mark_stopped(&EnvState::QuotaExceeded));
    }

    #[test]
    fn stop_state_update_preserves_terminal_env_states() {
        let mut failed = test_env(EnvState::Failed);
        apply_stopped_state(&mut failed);
        assert_eq!(failed.state, EnvState::Failed);

        let mut quota = test_env(EnvState::QuotaExceeded);
        apply_stopped_state(&mut quota);
        assert_eq!(quota.state, EnvState::QuotaExceeded);

        let mut running = test_env(EnvState::Running);
        apply_stopped_state(&mut running);
        assert_eq!(running.state, EnvState::Stopped);
    }

    #[test]
    fn idle_timeout_only_expires_running_envs_after_inactivity_window() {
        let now = Utc::now();
        let mut env = test_env(EnvState::Running);
        env.limits.idle_timeout = "30m".to_string();
        env.last_active_at = now - chrono::Duration::minutes(31);
        assert!(idle_timeout_expired(&env, now));

        env.last_active_at = now - chrono::Duration::minutes(29);
        assert!(!idle_timeout_expired(&env, now));

        env.state = EnvState::Stopped;
        env.last_active_at = now - chrono::Duration::hours(1);
        assert!(!idle_timeout_expired(&env, now));

        env.state = EnvState::Running;
        env.limits.idle_timeout = "0".to_string();
        assert!(!idle_timeout_expired(&env, now));
    }

    #[test]
    fn base_source_labels_current_project_vm_root() {
        assert_eq!(base_source_label(Path::new("/")), "current-project-vm");
        assert_eq!(base_source_label(Path::new("/mnt/rootfs")), "/mnt/rootfs");
    }

    #[test]
    fn running_env_guard_blocks_inactive_envs() {
        let mut env = test_env(EnvState::Running);
        ensure_running_env(&env).unwrap();

        env.state = EnvState::Stopped;
        assert!(ensure_running_env(&env)
            .unwrap_err()
            .to_string()
            .contains("start it before running commands or sessions"));

        env.state = EnvState::QuotaExceeded;
        assert!(ensure_running_env(&env).is_err());
    }

    #[test]
    fn duplicate_session_create_only_rejects_running_existing_sessions() {
        assert!(should_reject_session_create(true, true));
        assert!(!should_reject_session_create(true, false));
        assert!(!should_reject_session_create(false, false));
    }

    #[test]
    fn session_list_repairs_env_session_index() {
        let mut env = test_env(EnvState::Running);
        env.sessions = vec!["stale".to_string()];
        let sessions = vec![
            test_session("codex-1", "dev"),
            test_session("codex-1", "codex"),
        ];

        assert!(sync_env_session_index(&mut env, &sessions));
        assert_eq!(env.sessions, vec!["codex".to_string(), "dev".to_string()]);
        assert!(!sync_env_session_index(&mut env, &sessions));
    }

    #[test]
    fn default_shell_uses_absolute_bash_path() {
        assert_eq!(default_shell_command(), vec!["/bin/bash".to_string()]);
    }

    #[test]
    fn overlay_changed_paths_report_upper_entries_and_whiteouts() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home/user")).unwrap();
        fs::write(dir.path().join("home/user/edited.txt"), "changed").unwrap();
        fs::write(dir.path().join("home/user/.wh.deleted.txt"), "").unwrap();
        fs::write(dir.path().join("home/user/.wh..wh..opq"), "").unwrap();

        let mut paths = Vec::new();
        collect_overlay_changed_paths(dir.path(), dir.path(), &mut paths).unwrap();
        paths.sort();

        assert_eq!(
            paths,
            vec![
                "/home".to_string(),
                "/home/user".to_string(),
                "/home/user/deleted.txt".to_string(),
                "/home/user/edited.txt".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn base_cleanup_removes_host_agentfs_state() {
        let dir = tempfile::tempdir().unwrap();
        for rel in [
            "proc",
            "sys",
            "dev",
            "run",
            "tmp",
            "agentfs/bases/base-001",
            "agentfs/envs/sibling",
            "agentfs/cache/apt",
            "agentfs/runtime/sockets",
            "agentfs/stray",
        ] {
            fs::create_dir_all(dir.path().join(rel)).unwrap();
        }
        fs::write(dir.path().join("agentfs/bases/base-001/secret"), "").unwrap();
        fs::write(dir.path().join("agentfs/envs/sibling/secret"), "").unwrap();
        fs::write(dir.path().join("agentfs/stray/file"), "").unwrap();

        let service = AgentService::new(AgentConfig::new(dir.path().join("agentfs-host")));
        service.clean_runtime_paths(dir.path()).await.unwrap();

        assert!(dir.path().join("proc").is_dir());
        assert!(dir.path().join("sys").is_dir());
        assert!(dir.path().join("dev").is_dir());
        assert!(dir.path().join("run").is_dir());
        assert!(dir.path().join("tmp").is_dir());
        assert_eq!(
            fs::metadata(dir.path().join("tmp"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o1777
        );
        assert!(dir.path().join("agentfs").is_dir());
        assert!(!dir.path().join("agentfs/bases").exists());
        assert!(!dir.path().join("agentfs/envs").exists());
        assert!(!dir.path().join("agentfs/cache").exists());
        assert!(!dir.path().join("agentfs/runtime").exists());
        assert!(!dir.path().join("agentfs/stray").exists());
    }

    #[tokio::test]
    async fn failed_env_dir_cleanup_removes_partial_metadata_tree() {
        let dir = tempfile::tempdir().unwrap();
        let env_dir = dir.path().join("envs/codex-1");
        fs::create_dir_all(env_dir.join("logs/sessions")).unwrap();
        fs::create_dir_all(env_dir.join("sessions")).unwrap();
        fs::write(env_dir.join("logs/lifecycle.log"), "creating\n").unwrap();

        cleanup_failed_env_dir(&env_dir).await;

        assert!(!env_dir.exists());
    }

    #[tokio::test]
    async fn failed_base_dir_cleanup_removes_partial_metadata_tree() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join("bases/base-001");
        fs::create_dir_all(base_dir.join("rootfs")).unwrap();
        fs::write(base_dir.join("dpkg.list"), "bash 1.0\n").unwrap();
        fs::write(base_dir.join("created_at"), "now").unwrap();

        cleanup_failed_base_dir(&base_dir).await;

        assert!(!base_dir.exists());
    }

    #[tokio::test]
    async fn directory_cleanup_tolerates_already_removed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let env_dir = dir.path().join("envs/codex-1");
        fs::create_dir_all(env_dir.join("logs/sessions")).unwrap();
        fs::write(env_dir.join("logs/sessions/dev.log"), "done\n").unwrap();

        remove_dir_all_if_exists(&env_dir).await.unwrap();
        remove_dir_all_if_exists(&env_dir).await.unwrap();

        assert!(!env_dir.exists());
    }

    #[tokio::test]
    async fn path_cleanup_removes_files_dirs_and_symlinks_without_following() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        let nested = dir.path().join("nested");
        let target = dir.path().join("target");
        let link = dir.path().join("link");

        fs::write(&file, "file").unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("child"), "child").unwrap();
        fs::write(&target, "target").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_path_if_exists(&file).await.unwrap();
        remove_path_if_exists(&nested).await.unwrap();
        remove_path_if_exists(&link).await.unwrap();
        remove_path_if_exists(&dir.path().join("missing"))
            .await
            .unwrap();

        assert!(!file.exists());
        assert!(!nested.exists());
        assert!(!link.exists());
        assert!(target.exists());
    }

    #[tokio::test]
    async fn persisted_session_logs_are_available_offline() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("sessions/dev.log");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        fs::write(&log_path, "hello from tmux\n").unwrap();

        assert_eq!(
            read_session_log_file(&log_path).await.unwrap(),
            "hello from tmux\n"
        );
        assert_eq!(
            read_session_log_file(&dir.path().join("missing.log"))
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn offline_session_logs_sync_from_child_rootfs_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let child_transcript = dir
            .path()
            .join("rootfs/var/log/agent-forkd/sessions/dev.log");
        let agentfs_log = dir
            .path()
            .join("agentfs/envs/codex-1/logs/sessions/dev.log");
        fs::create_dir_all(child_transcript.parent().unwrap()).unwrap();
        fs::write(&child_transcript, "persisted in child rootfs\n").unwrap();

        let logs = read_offline_session_log(&child_transcript, &agentfs_log)
            .await
            .unwrap();

        assert_eq!(logs, "persisted in child rootfs\n");
        assert_eq!(
            fs::read_to_string(agentfs_log).unwrap(),
            "persisted in child rootfs\n"
        );
    }

    #[tokio::test]
    async fn offline_session_logs_fall_back_to_agentfs_copy() {
        let dir = tempfile::tempdir().unwrap();
        let child_transcript = dir
            .path()
            .join("rootfs/var/log/agent-forkd/sessions/dev.log");
        let agentfs_log = dir
            .path()
            .join("agentfs/envs/codex-1/logs/sessions/dev.log");
        fs::create_dir_all(agentfs_log.parent().unwrap()).unwrap();
        fs::write(&agentfs_log, "already synced\n").unwrap();

        let logs = read_offline_session_log(&child_transcript, &agentfs_log)
            .await
            .unwrap();

        assert_eq!(logs, "already synced\n");
    }

    fn test_env(state: EnvState) -> Env {
        Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            backend: RootfsBackend::Btrfs,
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            network_policy: Default::default(),
            limits: Limits::default(),
            sessions: Vec::new(),
        }
    }

    fn test_session(env_id: &str, session_id: &str) -> Session {
        Session {
            id: session_id.to_string(),
            env_id: env_id.to_string(),
            command: "bash".to_string(),
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path: format!("/agentfs/envs/{env_id}/logs/sessions/{session_id}.log").into(),
        }
    }

    fn write_executable(path: &Path) {
        fs::write(path, "").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
