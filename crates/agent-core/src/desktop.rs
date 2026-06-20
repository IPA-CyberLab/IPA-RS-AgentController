#[cfg(target_os = "macos")]
use crate::command::macos_sandbox_profile;
#[cfg(windows)]
use crate::command::{forget_desktop_session_job, terminate_desktop_session_job};
#[cfg(target_os = "macos")]
use crate::command::{shell_join, shell_quote};
use crate::command::{CmdOutput, CommandRunner};
use crate::config::AgentConfig;
use crate::export::{ExportType, Exporter};
use crate::model::{
    machine_name, Base, Env, EnvState, EnvStatus, LimitOverrides, Limits, RootfsBackend, Session,
    SessionState, SessionType,
};
use crate::path_overlay::{absolute_path_as_overlay_relative, visible_path_from_whiteout_storage};
use crate::protocol::{Request, Response};
use crate::reflink;
use crate::storage::{validate_id, write_text_file, Layout};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct DesktopService {
    pub config: AgentConfig,
    layout: Layout,
    exporter: Exporter,
    runner: CommandRunner,
}

fn path_preserving_cwd_for_backend(
    backend: RootfsBackend,
    base_source: &str,
    cwd: Option<&Path>,
) -> PathBuf {
    let base_source_path = Path::new(base_source);
    match backend {
        RootfsBackend::PathPreservingOverlay | RootfsBackend::WindowsMinifilterOverlay => cwd
            .filter(|cwd| cwd.starts_with(base_source_path))
            .unwrap_or(base_source_path)
            .to_path_buf(),
        _ => cwd.unwrap_or(base_source_path).to_path_buf(),
    }
}

impl DesktopService {
    pub fn new(config: AgentConfig) -> Self {
        let layout = Layout::new(config.agentfs.clone());
        Self {
            config,
            layout,
            exporter: Exporter::default(),
            runner: CommandRunner,
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.layout.ensure_agentfs().await
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
                DesktopService::new(AgentConfig::new(agentfs))
                    .init()
                    .await?;
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
                cwd,
            } => {
                self.new_target(
                    &target,
                    &base,
                    &from,
                    backend,
                    &profile,
                    limits,
                    &command,
                    cwd.as_deref(),
                )
                .await
            }
            Request::BaseFreeze {
                name,
                from,
                backend,
            } => {
                match backend {
                    Some(backend) => {
                        self.base_freeze_with_backend(&name, &from, backend).await?;
                    }
                    None => {
                        self.base_freeze(&name, &from).await?;
                    }
                }
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
            Request::Exec { id, command, cwd } => {
                let output = self.exec(&id, &command, cwd.as_deref()).await?;
                Ok(Response::Exec {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Request::Shell {
                id,
                cwd,
                persistent,
            } => {
                let env = self.shell_target(&id).await?;
                let base = self.layout.read_base(&env.base_id).await?;
                let preserved_cwd = path_preserving_cwd_for_backend(
                    env.backend.clone(),
                    &base.source,
                    cwd.as_deref(),
                );
                let preserved_cwd = preserved_cwd.to_string_lossy();
                Ok(Response::DesktopShell {
                    command: desktop_shell_command(
                        &env.rootfs_path,
                        Path::new(&base.source),
                        env.backend.clone(),
                        &env.id,
                        Some(&preserved_cwd),
                        &env.limits,
                        persistent,
                    ),
                    rootfs_path: env.rootfs_path,
                })
            }
            Request::Ping => Ok(Response::Ok),
            Request::SessionCreate {
                env_id,
                session_id,
                command,
                cwd,
            } => {
                self.session_create(&env_id, &session_id, &command, cwd.as_deref())
                    .await?;
                Ok(Response::Ok)
            }
            Request::SessionAttach { .. } => Err(anyhow!(
                "interactive attach is not implemented by the native desktop backend yet"
            )),
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
            Request::Apply { env_id, force } => Ok(Response::Text {
                text: self.apply(&env_id, force).await?,
            }),
        }
    }

    pub async fn new_target(
        &self,
        target: &str,
        base_id: &str,
        from: &Path,
        backend: Option<RootfsBackend>,
        profile_name: &str,
        limit_overrides: LimitOverrides,
        command: &[String],
        cwd: Option<&Path>,
    ) -> Result<Response> {
        self.init().await?;
        if tokio::fs::try_exists(self.layout.env_dir(target).join("meta.json")).await? {
            self.ensure_env_started(target).await?;
            return self.target_response(target, command, cwd).await;
        }
        self.ensure_base(base_id, from, backend).await?;
        self.ensure_env(target, base_id, profile_name, limit_overrides)
            .await?;
        self.ensure_env_started(target).await?;
        self.target_response(target, command, cwd).await
    }

    async fn target_response(
        &self,
        target: &str,
        command: &[String],
        cwd: Option<&Path>,
    ) -> Result<Response> {
        if command.is_empty() {
            let env = self.shell_target(target).await?;
            let base = self.layout.read_base(&env.base_id).await?;
            let preserved_cwd =
                path_preserving_cwd_for_backend(env.backend.clone(), &base.source, cwd);
            let preserved_cwd = preserved_cwd.to_string_lossy();
            Ok(Response::DesktopShell {
                command: desktop_shell_command(
                    &env.rootfs_path,
                    Path::new(&base.source),
                    env.backend.clone(),
                    &env.id,
                    Some(&preserved_cwd),
                    &env.limits,
                    false,
                ),
                rootfs_path: env.rootfs_path,
            })
        } else {
            let output = self.exec(target, command, cwd).await?;
            Ok(Response::Exec {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
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
        if !backend.is_desktop_native() {
            return Err(anyhow!(
                "backend {backend:?} is not a desktop clone backend"
            ));
        }
        if backend.is_path_preserving_overlay() {
            self.ensure_safe_path_preserving_source(from)?;
        }
        let base_dir = self.layout.base_dir(name);
        if base_dir.exists() {
            return Err(anyhow!("base {name} already exists"));
        }
        tokio::fs::create_dir_all(&base_dir).await?;
        let rootfs = match backend {
            RootfsBackend::PathPreservingOverlay | RootfsBackend::WindowsMinifilterOverlay => {
                self.layout.base_lower(name)
            }
            _ => self.layout.base_rootfs(name),
        };
        let clone_target = desktop_base_clone_target(&backend, &rootfs, from)?;
        if let Err(error) = clone_desktop_tree(from, &clone_target) {
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

    fn ensure_safe_path_preserving_source(&self, from: &Path) -> Result<()> {
        if !from.is_absolute() {
            bail!(
                "path-preserving overlay source must be absolute: {}",
                from.display()
            );
        }
        let canonical = std::fs::canonicalize(from)
            .with_context(|| format!("failed to resolve source {}", from.display()))?;
        if !canonical.is_dir() {
            bail!(
                "path-preserving overlay source must be a directory: {}",
                from.display()
            );
        }
        if canonical == Path::new("/") {
            bail!("refusing to create path-preserving overlay from filesystem root /; cd into a project directory or pass --from explicitly");
        }
        let broad_roots = [
            "/Users", "/System", "/Library", "/bin", "/sbin", "/usr", "/etc", "/var", "/private",
            "/tmp", "/dev", "/Volumes",
        ];
        if broad_roots.iter().any(|root| canonical == Path::new(root)) {
            bail!(
                "refusing to create path-preserving overlay from broad system directory {}; cd into a project directory",
                canonical.display()
            );
        }
        if let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| std::fs::canonicalize(home).ok())
        {
            if canonical == home {
                bail!(
                    "refusing to create path-preserving overlay from home directory {}; cd into a project directory",
                    canonical.display()
                );
            }
        }
        if let Ok(agentfs) = std::fs::canonicalize(&self.config.agentfs) {
            if canonical == agentfs || canonical.starts_with(&agentfs) {
                bail!(
                    "refusing to create path-preserving overlay from agentfs storage {}; use the original host project path",
                    canonical.display()
                );
            }
        }
        Ok(())
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
        if !base.backend.is_desktop_native() {
            return Err(anyhow!("base {base_id} is not a desktop native base"));
        }
        let env_dir = self.layout.env_dir(id);
        if env_dir.exists() {
            return Err(anyhow!("env {id} already exists"));
        }
        tokio::fs::create_dir_all(&env_dir).await?;
        let rootfs = match base.backend {
            RootfsBackend::PathPreservingOverlay | RootfsBackend::WindowsMinifilterOverlay => {
                let lower = self.layout.env_lower(id);
                if let Err(error) = clone_desktop_tree(&base.rootfs_path, &lower) {
                    let _ = remove_dir_all_if_exists(&env_dir).await;
                    return Err(error);
                }
                self.ensure_path_preserving_overlay_dirs(id).await?;
                self.layout.env_view_root(id)
            }
            _ => {
                let rootfs = self.layout.env_rootfs(id);
                if let Err(error) = clone_desktop_tree(&base.rootfs_path, &rootfs) {
                    let _ = remove_dir_all_if_exists(&env_dir).await;
                    return Err(error);
                }
                rootfs
            }
        };
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
        validate_id(id)?;
        let env = match self.layout.read_env(id).await {
            Ok(env) => env,
            Err(error) => {
                let env_dir = self.layout.env_dir(id);
                let metadata = env_dir.join("meta.json");
                if tokio::fs::try_exists(&env_dir).await?
                    && !tokio::fs::try_exists(&metadata).await?
                {
                    return remove_dir_all_if_exists(&env_dir).await;
                }
                return Err(error);
            }
        };
        ensure_desktop_backend(&env)?;
        if env.backend.is_path_preserving_overlay() {
            unmount_macos_overlay_if_active(&env.rootfs_path)?;
        }
        remove_dir_all_if_exists(&self.layout.env_dir(id)).await
    }

    pub async fn env_status(&self, id: &str) -> Result<EnvStatus> {
        let env = self.layout.read_env(id).await?;
        let disk_used = dir_size(&env.rootfs_path).ok().map(human_bytes);
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

    pub async fn exec(
        &self,
        id: &str,
        command: &[String],
        cwd: Option<&Path>,
    ) -> Result<CmdOutput> {
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
            .run_desktop_env_command(&env, program, args, cwd)
            .await?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(output)
    }

    pub async fn session_create(
        &self,
        env_id: &str,
        session_id: &str,
        command: &[String],
        cwd: Option<&Path>,
    ) -> Result<()> {
        validate_id(session_id)?;
        if command.is_empty() {
            return Err(anyhow!("session command cannot be empty"));
        }
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_desktop_env(&env)?;
        let metadata_path = self
            .layout
            .sessions_dir(env_id)
            .join(format!("{session_id}.json"));
        let metadata_exists = tokio::fs::try_exists(&metadata_path).await?;
        let running = match self.layout.read_session(env_id, session_id).await {
            Ok(_) => self.session_process_running(env_id, session_id).await?,
            Err(_) if !metadata_exists => false,
            Err(error) => {
                return Err(error.context(format!(
                    "session {session_id} metadata exists but could not be read"
                )));
            }
        };
        if metadata_exists && running {
            return Err(anyhow!(
                "session {session_id} is already running in env {env_id}"
            ));
        }
        let (program, args) = command
            .split_first()
            .ok_or_else(|| anyhow!("session command cannot be empty"))?;
        let log_path = desktop_session_log_path(&self.layout, env_id, session_id);
        let pid = self
            .spawn_desktop_env_session(&env, program, args, &log_path, cwd)
            .await?;
        write_text_file(
            &desktop_session_pid_path(&self.layout, env_id, session_id),
            pid.to_string(),
        )
        .await?;
        CommandRunner::append_to_file(
            &log_path,
            &format!("created native desktop session {session_id} pid {pid}\n"),
        )
        .await?;
        let session = Session {
            id: session_id.to_string(),
            env_id: env_id.to_string(),
            command: command_display(command),
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path,
        };
        if !env.sessions.iter().any(|existing| existing == session_id) {
            env.sessions.push(session_id.to_string());
            env.sessions.sort();
        }
        env.last_active_at = Utc::now();
        self.layout.write_session(&session).await?;
        self.layout.write_env(&env).await?;
        Ok(())
    }

    pub async fn session_detach(&self, env_id: &str, session_id: &str) -> Result<()> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_desktop_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        if !self.session_process_running(env_id, session_id).await? {
            session.state = SessionState::Stopped;
            self.layout.write_session(&session).await?;
            return Err(anyhow!(
                "session {session_id} in env {env_id} is not running"
            ));
        }
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await
    }

    pub async fn session_kill(&self, env_id: &str, session_id: &str) -> Result<()> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_running_desktop_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        if let Some(pid) = read_desktop_session_pid(&self.layout, env_id, session_id).await? {
            kill_process_tree(&self.runner, pid).await?;
        }
        remove_file_if_exists(&desktop_session_pid_path(&self.layout, env_id, session_id)).await?;
        session.state = SessionState::Stopped;
        env.last_active_at = Utc::now();
        self.layout.write_session(&session).await?;
        self.layout.write_env(&env).await
    }

    pub async fn session_list(&self, env_id: &str) -> Result<Vec<Session>> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_desktop_backend(&env)?;
        let mut sessions = self.layout.list_sessions(env_id).await?;
        for session in &mut sessions {
            session.state = if env.state == EnvState::Running
                && self.session_process_running(env_id, &session.id).await?
            {
                SessionState::Running
            } else {
                SessionState::Stopped
            };
            self.layout.write_session(session).await?;
        }
        if sync_env_session_index(&mut env, &sessions) {
            self.layout.write_env(&env).await?;
        }
        Ok(sessions)
    }

    pub async fn session_logs(&self, env_id: &str, session_id: &str) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_desktop_backend(&env)?;
        let session = self.layout.read_session(env_id, session_id).await?;
        let text = read_text_file_or_empty(&session.log_path).await?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(text)
    }

    async fn session_process_running(&self, env_id: &str, session_id: &str) -> Result<bool> {
        let Some(pid) = read_desktop_session_pid(&self.layout, env_id, session_id).await? else {
            return Ok(false);
        };
        process_running(&self.runner, pid).await
    }

    pub async fn diff(&self, env_id: &str) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_desktop_backend(&env)?;
        if env.state != EnvState::Running {
            return Err(anyhow!("env {env_id} is not running"));
        }
        let text = self.exporter.workspace_patch(&env).await?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(text)
    }

    pub async fn export(&self, env_id: &str, export_type: ExportType) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_desktop_backend(&env)?;
        if env.state != EnvState::Running {
            return Err(anyhow!("env {env_id} is not running"));
        }
        let base = self.layout.read_base(&env.base_id).await?;
        let text = match export_type {
            ExportType::WorkspacePatch => self.exporter.workspace_patch(&env).await?,
            ExportType::RootfsChangedPaths => {
                if env.backend.is_path_preserving_overlay() {
                    self.path_preserving_overlay_changed_paths(env_id)?
                } else {
                    Exporter::changed_paths_by_walk(&base.rootfs_path, &env.rootfs_path)?
                }
            }
            ExportType::DpkgDelta => {
                return Err(anyhow!(
                    "dpkg-delta is not implemented by the native desktop backend"
                ));
            }
        };
        let artifact = self
            .layout
            .env_dir(env_id)
            .join("exports")
            .join(export_type.artifact_name());
        write_text_file(&artifact, &text).await?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(text)
    }

    pub async fn apply(&self, env_id: &str, force: bool) -> Result<String> {
        let mut env = self.layout.read_env(env_id).await?;
        ensure_desktop_backend(&env)?;
        if !env.backend.is_path_preserving_overlay() {
            bail!("apply is implemented only for path-preserving overlay envs");
        }
        let base = self.layout.read_base(&env.base_id).await?;
        let stats = self.apply_path_preserving_overlay(&env, Path::new(&base.source), force)?;
        env.last_active_at = Utc::now();
        self.layout.write_env(&env).await?;
        Ok(stats.summary())
    }

    async fn ensure_base(
        &self,
        base_id: &str,
        from: &Path,
        backend: Option<RootfsBackend>,
    ) -> Result<Base> {
        validate_id(base_id)?;
        let manifest = self.layout.base_dir(base_id).join("manifest.json");
        if tokio::fs::try_exists(&manifest).await? {
            let base = self.layout.read_base(base_id).await?;
            if let Some(requested_backend) = backend {
                if base.backend != requested_backend {
                    return Err(anyhow!(
                        "base {base_id} was created with backend {:?}; requested backend {:?}; use a different --base or omit --backend",
                        base.backend,
                        requested_backend
                    ));
                }
            }
            if base.backend.is_path_preserving_overlay()
                && base.source != from.display().to_string()
            {
                return Err(anyhow!(
                    "base {base_id} was created from {}; requested source is {}; use a different --base or omit --base for the source-specific default",
                    base.source,
                    from.display()
                ));
            }
            return Ok(base);
        }
        match backend {
            Some(backend) => self.base_freeze_with_backend(base_id, from, backend).await,
            None => self.base_freeze(base_id, from).await,
        }
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

    async fn shell_target(&self, id: &str) -> Result<Env> {
        let env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        if env.state != EnvState::Running {
            return Err(anyhow!("env {id} is not running"));
        }
        Ok(env)
    }

    async fn ensure_path_preserving_overlay_dirs(&self, id: &str) -> Result<()> {
        for dir in [
            self.layout.env_upper(id),
            self.layout.env_whiteouts(id),
            self.layout.env_view_root(id),
        ] {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    async fn run_desktop_env_command(
        &self,
        env: &Env,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<CmdOutput> {
        if env.backend == RootfsBackend::PathPreservingOverlay {
            let base = self.layout.read_base(&env.base_id).await?;
            let preserved_cwd =
                path_preserving_cwd_for_backend(env.backend.clone(), &base.source, cwd);
            return self
                .runner
                .run_macos_path_preserving_overlay(
                    &env.rootfs_path,
                    Path::new(&base.source),
                    &self.layout.env_lower(&env.id),
                    &self.layout.env_upper(&env.id),
                    &self.layout.env_whiteouts(&env.id),
                    &preserved_cwd,
                    program,
                    args,
                    &env.limits,
                )
                .await;
        }
        if env.backend == RootfsBackend::WindowsMinifilterOverlay {
            let base = self.layout.read_base(&env.base_id).await?;
            let preserved_cwd =
                path_preserving_cwd_for_backend(env.backend.clone(), &base.source, cwd);
            return self
                .runner
                .run_windows_minifilter_overlay(
                    &env.id,
                    Path::new(&base.source),
                    &self.layout.env_lower(&env.id),
                    &self.layout.env_upper(&env.id),
                    &self.layout.env_whiteouts(&env.id),
                    &preserved_cwd,
                    program,
                    args,
                    &env.limits,
                )
                .await;
        }
        self.runner
            .run_desktop_isolated(&env.rootfs_path, program, args, &env.limits)
            .await
    }

    async fn spawn_desktop_env_session(
        &self,
        env: &Env,
        program: &str,
        args: &[String],
        log_path: &Path,
        cwd: Option<&Path>,
    ) -> Result<u32> {
        if env.backend == RootfsBackend::PathPreservingOverlay {
            let base = self.layout.read_base(&env.base_id).await?;
            let preserved_cwd =
                path_preserving_cwd_for_backend(env.backend.clone(), &base.source, cwd);
            return self.runner.spawn_macos_path_preserving_overlay_session(
                &env.rootfs_path,
                Path::new(&base.source),
                &self.layout.env_lower(&env.id),
                &self.layout.env_upper(&env.id),
                &self.layout.env_whiteouts(&env.id),
                &preserved_cwd,
                program,
                args,
                log_path,
                &env.limits,
            );
        }
        if env.backend == RootfsBackend::WindowsMinifilterOverlay {
            let base = self.layout.read_base(&env.base_id).await?;
            let preserved_cwd =
                path_preserving_cwd_for_backend(env.backend.clone(), &base.source, cwd);
            return self.runner.spawn_windows_minifilter_overlay_session(
                &env.id,
                Path::new(&base.source),
                &self.layout.env_lower(&env.id),
                &self.layout.env_upper(&env.id),
                &self.layout.env_whiteouts(&env.id),
                &preserved_cwd,
                program,
                args,
                log_path,
                &env.limits,
            );
        }
        self.runner
            .spawn_desktop_session(&env.rootfs_path, program, args, log_path, &env.limits)
    }

    fn path_preserving_overlay_changed_paths(&self, id: &str) -> Result<String> {
        let mut paths = Vec::new();
        collect_path_preserving_overlay_entries(
            &self.layout.env_upper(id),
            &self.layout.env_upper(id),
            false,
            &mut paths,
        )?;
        collect_path_preserving_overlay_entries(
            &self.layout.env_whiteouts(id),
            &self.layout.env_whiteouts(id),
            true,
            &mut paths,
        )?;
        paths.sort();
        Ok(paths.join("\n"))
    }

    fn apply_path_preserving_overlay(
        &self,
        env: &Env,
        source_root: &Path,
        force: bool,
    ) -> Result<ApplyStats> {
        let lower = self.layout.env_lower(&env.id);
        let upper = self.layout.env_upper(&env.id);
        let whiteouts = self.layout.env_whiteouts(&env.id);
        let source_rel = absolute_path_as_overlay_relative(source_root)?;
        let upper_source = upper.join(&source_rel);
        let whiteouts_source = whiteouts.join(&source_rel);
        let mut stats = ApplyStats::default();

        let mut upper_entries = collect_overlay_apply_entries(&upper_source)?;
        upper_entries.sort_by_key(|path| path.components().count());
        let mut applied_upper = Vec::new();
        for upper_path in &upper_entries {
            let visible = visible_path_from_overlay_storage(&upper, upper_path)?;
            if !visible.starts_with(source_root) {
                continue;
            }
            let target = visible.clone();
            let lower_path = lower.join(absolute_path_as_overlay_relative(&visible)?);
            ensure_no_host_conflict(&lower_path, &target, force)?;
            apply_upper_path(upper_path, &target)?;
            apply_upper_path(upper_path, &lower_path)?;
            stats.updated += 1;
            applied_upper.push(upper_path.clone());
        }
        cleanup_applied_overlay_entries(applied_upper, &upper)?;

        let mut whiteout_entries = collect_overlay_apply_entries(&whiteouts_source)?;
        whiteout_entries.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        let mut applied_whiteouts = Vec::new();
        for whiteout_path in &whiteout_entries {
            let Some(metadata) = symlink_metadata_if_exists(whiteout_path)? else {
                continue;
            };
            if metadata.is_dir() {
                continue;
            }
            let visible = visible_path_from_whiteout_storage(&whiteouts, whiteout_path)?;
            if !visible.starts_with(source_root) {
                continue;
            }
            let target = visible.clone();
            let lower_path = lower.join(absolute_path_as_overlay_relative(&visible)?);
            ensure_no_host_conflict(&lower_path, &target, force)?;
            remove_path_recursively_if_exists(&target)?;
            remove_path_recursively_if_exists(&lower_path)?;
            stats.deleted += 1;
            applied_whiteouts.push(whiteout_path.clone());
        }
        cleanup_applied_overlay_entries(applied_whiteouts, &whiteouts)?;

        Ok(stats)
    }
}

fn ensure_desktop_backend(env: &Env) -> Result<()> {
    if env.backend.is_desktop_native() {
        Ok(())
    } else {
        Err(anyhow!("env {} is not a desktop native env", env.id))
    }
}

fn ensure_running_desktop_env(env: &Env) -> Result<()> {
    ensure_desktop_backend(env)?;
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

fn desktop_session_log_path(layout: &Layout, env_id: &str, session_id: &str) -> PathBuf {
    layout
        .session_logs(env_id)
        .join(format!("{session_id}.log"))
}

fn desktop_session_pid_path(layout: &Layout, env_id: &str, session_id: &str) -> PathBuf {
    layout
        .sessions_dir(env_id)
        .join(format!("{session_id}.pid"))
}

async fn read_desktop_session_pid(
    layout: &Layout,
    env_id: &str,
    session_id: &str,
) -> Result<Option<u32>> {
    let path = desktop_session_pid_path(layout, env_id, session_id);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            text.trim().parse::<u32>().map(Some).with_context(|| {
                format!("invalid native desktop session pid in {}", path.display())
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn read_text_file_or_empty(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_file_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
async fn process_running(runner: &CommandRunner, pid: u32) -> Result<bool> {
    if pid <= 1 {
        return Ok(false);
    }
    let output = runner
        .run("kill", vec!["-0".to_string(), pid.to_string()])
        .await?;
    if output.status == 0 {
        return Ok(true);
    }
    process_group_running(runner, pid).await
}

#[cfg(windows)]
async fn process_running(runner: &CommandRunner, pid: u32) -> Result<bool> {
    let output = runner
        .run(
            "tasklist",
            vec![
                "/FI".to_string(),
                format!("PID eq {pid}"),
                "/NH".to_string(),
            ],
        )
        .await?;
    let running = output.status == 0
        && output
            .stdout
            .split_whitespace()
            .any(|field| field == pid.to_string());
    if !running {
        forget_desktop_session_job(pid);
    }
    Ok(running)
}

#[cfg(unix)]
async fn kill_process_tree(runner: &CommandRunner, pid: u32) -> Result<()> {
    ensure_safe_desktop_session_pid(pid)?;

    if terminate_process_group(runner, pid).await? {
        return Ok(());
    }

    let mut targets = process_descendants(runner, pid).await?;
    targets.push(pid);
    targets.sort_unstable();
    targets.dedup();

    for target in targets.into_iter().rev() {
        terminate_pid(runner, target).await?;
    }
    Ok(())
}

#[cfg(unix)]
async fn process_group_running(runner: &CommandRunner, pgid: u32) -> Result<bool> {
    ensure_safe_desktop_session_pid(pgid)?;
    let Ok(output) = runner
        .run(
            "ps",
            vec![
                "-o".to_string(),
                "pid=".to_string(),
                "-g".to_string(),
                pgid.to_string(),
            ],
        )
        .await
    else {
        return Ok(false);
    };
    Ok(output.status == 0 && output.stdout.lines().any(|line| !line.trim().is_empty()))
}

#[cfg(unix)]
async fn terminate_process_group(runner: &CommandRunner, pgid: u32) -> Result<bool> {
    ensure_safe_desktop_session_pid(pgid)?;
    let output = runner
        .run("kill", vec!["-TERM".to_string(), format!("-{pgid}")])
        .await?;
    Ok(output.status == 0)
}

#[cfg(unix)]
fn ensure_safe_desktop_session_pid(pid: u32) -> Result<()> {
    if pid <= 1 {
        bail!("refusing to signal unsafe native desktop session pid {pid}");
    }
    Ok(())
}

#[cfg(unix)]
async fn process_descendants(runner: &CommandRunner, pid: u32) -> Result<Vec<u32>> {
    let output = runner
        .run("ps", vec!["-eo".to_string(), "pid=,ppid=".to_string()])
        .await?;
    if output.status != 0 {
        return Err(anyhow!(
            "failed to list native desktop session descendants for pid {pid}: {}{}",
            output.stdout,
            output.stderr
        ));
    }
    Ok(descendant_pids_from_ps(&output.stdout, pid))
}

#[cfg(unix)]
fn descendant_pids_from_ps(ps_output: &str, root_pid: u32) -> Vec<u32> {
    let processes = ps_output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            Some((pid, ppid))
        })
        .collect::<Vec<_>>();

    let mut descendants = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = vec![root_pid];
    while let Some(parent) = queue.pop() {
        for (pid, ppid) in &processes {
            if *ppid == parent && seen.insert(*pid) {
                descendants.push(*pid);
                queue.push(*pid);
            }
        }
    }
    descendants
}

#[cfg(unix)]
async fn terminate_pid(runner: &CommandRunner, pid: u32) -> Result<()> {
    ensure_safe_desktop_session_pid(pid)?;
    let output = runner
        .run("kill", vec!["-TERM".to_string(), pid.to_string()])
        .await?;
    if output.status == 0 || !process_running(runner, pid).await? {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to kill native desktop session pid {pid}: {}{}",
            output.stdout,
            output.stderr
        ))
    }
}

#[cfg(windows)]
async fn kill_process_tree(runner: &CommandRunner, pid: u32) -> Result<()> {
    if terminate_desktop_session_job(pid)? {
        return Ok(());
    }
    let output = runner
        .run(
            "taskkill",
            vec![
                "/PID".to_string(),
                pid.to_string(),
                "/T".to_string(),
                "/F".to_string(),
            ],
        )
        .await?;
    if output.status == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to kill native desktop session pid {pid}: {}{}",
            output.stdout,
            output.stderr
        ))
    }
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

fn command_display(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./:=+".contains(&b))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    for attempt in 0..5 {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if directory_not_empty(&error) && cfg!(target_os = "macos") && attempt < 4 =>
            {
                remove_macos_directory_residue(path).await?;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn directory_not_empty(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::DirectoryNotEmpty
        || matches!(error.raw_os_error(), Some(39 | 66))
}

async fn remove_macos_directory_residue(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path.join(".DS_Store")).await {
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

fn collect_path_preserving_overlay_entries(
    root: &Path,
    dir: &Path,
    deleted: bool,
    paths: &mut Vec<String>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_path_preserving_overlay_entries(root, &path, deleted, paths)?;
            continue;
        }
        let visible_path = if deleted {
            visible_path_from_whiteout_storage(root, &path)?
        } else {
            visible_path_from_overlay_storage(root, &path)?
        };
        let visible = visible_path.display();
        if deleted {
            paths.push(format!("deleted {visible}"));
        } else {
            paths.push(visible.to_string());
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ApplyStats {
    updated: usize,
    deleted: usize,
}

impl ApplyStats {
    fn summary(&self) -> String {
        if self.updated == 0 && self.deleted == 0 {
            return "no changes to apply\n".to_string();
        }
        format!(
            "applied {} updated, {} deleted\n",
            self.updated, self.deleted
        )
    }
}

fn collect_overlay_apply_entries(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        entries.push(entry.path().to_path_buf());
    }
    Ok(entries)
}

fn visible_path_from_overlay_storage(root: &Path, path: &Path) -> Result<PathBuf> {
    let rel = path.strip_prefix(root)?;
    Ok(Path::new("/").join(rel))
}

fn ensure_no_host_conflict(lower: &Path, target: &Path, force: bool) -> Result<()> {
    if force || paths_equivalent(lower, target)? {
        return Ok(());
    }
    bail!(
        "host path {} changed since env creation; rerun with --force to overwrite",
        target.display()
    )
}

fn apply_upper_path(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)
        .with_context(|| format!("failed to stat {}", src.display()))?;
    if metadata.is_dir() {
        if dst.exists() && !std::fs::symlink_metadata(dst)?.is_dir() {
            remove_path_recursively_if_exists(dst)?;
        }
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create directory {}", dst.display()))?;
        copy_permissions(&metadata, dst)?;
        return Ok(());
    }
    replace_path_from(src, dst)
}

fn replace_path_from(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)
        .with_context(|| format!("failed to stat {}", src.display()))?;
    remove_path_recursively_if_exists(dst)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)
                .with_context(|| format!("failed to symlink {}", dst.display()))?;
        }
        #[cfg(not(unix))]
        bail!(
            "symlink apply is unsupported on this platform for {}",
            dst.display()
        );
    } else if metadata.is_file() {
        std::fs::copy(src, dst)
            .with_context(|| format!("failed to copy {} to {}", src.display(), dst.display()))?;
        copy_permissions(&metadata, dst)?;
    } else if metadata.is_dir() {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create directory {}", dst.display()))?;
        copy_permissions(&metadata, dst)?;
    }
    Ok(())
}

fn copy_permissions(metadata: &std::fs::Metadata, dst: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dst,
            std::fs::Permissions::from_mode(metadata.permissions().mode()),
        )
        .with_context(|| format!("failed to set permissions on {}", dst.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, dst);
    }
    Ok(())
}

fn cleanup_applied_overlay_entries(mut entries: Vec<PathBuf>, storage_root: &Path) -> Result<()> {
    entries.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in entries {
        remove_overlay_storage_entry(&path)?;
    }
    remove_empty_storage_parents(storage_root)?;
    Ok(())
}

fn remove_overlay_storage_entry(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if metadata.is_dir() {
        match std::fs::remove_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn remove_empty_storage_parents(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut dirs = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if entry.file_type().is_dir() {
            dirs.push(entry.path().to_path_buf());
        }
    }
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_path_recursively_if_exists(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))?;
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove file {}", path.display()))?;
    }
    Ok(())
}

fn paths_equivalent(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = symlink_metadata_if_exists(left)?;
    let right_meta = symlink_metadata_if_exists(right)?;
    let (left_meta, right_meta) = match (left_meta, right_meta) {
        (Some(left_meta), Some(right_meta)) => (left_meta, right_meta),
        (None, None) => return Ok(true),
        _ => return Ok(false),
    };
    if metadata_kind_changed(&left_meta, &right_meta)
        || metadata_permissions_changed(&left_meta, &right_meta)
    {
        return Ok(false);
    }
    if left_meta.is_file() {
        return files_equal(left, right);
    }
    if left_meta.file_type().is_symlink() {
        return Ok(std::fs::read_link(left)? == std::fs::read_link(right)?);
    }
    if left_meta.is_dir() {
        return dirs_equivalent(left, right);
    }
    Ok(true)
}

fn metadata_kind_changed(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.is_dir() != right.is_dir()
        || left.is_file() != right.is_file()
        || left.file_type().is_symlink() != right.file_type().is_symlink()
}

#[cfg(unix)]
fn metadata_permissions_changed(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.permissions().mode() != right.permissions().mode()
}

#[cfg(not(unix))]
fn metadata_permissions_changed(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = std::fs::metadata(left)?;
    let right_meta = std::fs::metadata(right)?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    Ok(std::fs::read(left)? == std::fs::read(right)?)
}

fn dirs_equivalent(left: &Path, right: &Path) -> Result<bool> {
    let mut names = BTreeSet::<OsString>::new();
    for entry in std::fs::read_dir(left)? {
        names.insert(entry?.file_name());
    }
    for entry in std::fs::read_dir(right)? {
        names.insert(entry?.file_name());
    }
    for name in names {
        if !paths_equivalent(&left.join(&name), &right.join(&name))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn symlink_metadata_if_exists(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn desktop_shell_command(
    rootfs_path: &Path,
    source_root: &Path,
    backend: RootfsBackend,
    env_id: &str,
    host_workspace: Option<&str>,
    limits: &Limits,
    persistent: bool,
) -> Vec<String> {
    platform_desktop_shell_command(
        rootfs_path,
        source_root,
        backend,
        env_id,
        host_workspace,
        limits,
        persistent,
    )
}

#[cfg(target_os = "macos")]
fn platform_desktop_shell_command(
    rootfs_path: &Path,
    source_root: &Path,
    backend: RootfsBackend,
    env_id: &str,
    host_workspace: Option<&str>,
    limits: &Limits,
    persistent: bool,
) -> Vec<String> {
    if backend == RootfsBackend::PathPreservingOverlay {
        return macos_path_preserving_shell_command(
            rootfs_path,
            source_root,
            env_id,
            host_workspace,
            limits,
            persistent,
        );
    }
    let tmpdir = rootfs_path.join(".tmp");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut command = vec![
        "sandbox-exec".to_string(),
        "-p".to_string(),
        macos_sandbox_profile(rootfs_path, &limits.network),
        "/usr/bin/env".to_string(),
        format!("HOME={}", rootfs_path.display()),
        format!("ZDOTDIR={}", rootfs_path.display()),
        format!("TMPDIR={}", tmpdir.display()),
        format!("AGENT_ENV_ID={env_id}"),
        format!("AGENT_NETWORK={}", limits.network),
    ];
    push_agent_prompt_env(&mut command, env_id, &shell);
    if let Ok(host_home) = std::env::var("HOME") {
        command.push(format!("HOST_HOME={host_home}"));
    }
    if let Some(host_workspace) = host_workspace {
        command.push(format!("HOST_WORKSPACE={host_workspace}"));
    }
    if persistent {
        push_persistent_agent_shell_command(&mut command, rootfs_path, shell);
    } else {
        push_agent_shell_command(&mut command, shell);
    }
    command
}

#[cfg(target_os = "macos")]
fn macos_path_preserving_shell_command(
    view_root: &Path,
    source_root: &Path,
    env_id: &str,
    host_workspace: Option<&str>,
    limits: &Limits,
    persistent: bool,
) -> Vec<String> {
    let preserved_cwd = host_workspace.unwrap_or("/");
    let env_dir = view_root.parent().unwrap_or_else(|| Path::new("/"));
    let lower = env_dir.join("lower");
    let upper = env_dir.join("upper");
    let whiteouts = env_dir.join("whiteouts");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut command = vec![
        macos_agent_viewd_program(),
        "shell".to_string(),
        "--view-root".to_string(),
        view_root.display().to_string(),
        "--source-root".to_string(),
        source_root.display().to_string(),
        "--lower".to_string(),
        lower.display().to_string(),
        "--upper".to_string(),
        upper.display().to_string(),
        "--whiteouts".to_string(),
        whiteouts.display().to_string(),
        "--cwd".to_string(),
        preserved_cwd.to_string(),
        "--env-id".to_string(),
        env_id.to_string(),
        "--network".to_string(),
        limits.network.clone(),
        "--".to_string(),
    ];
    if persistent {
        push_persistent_agent_shell_command(&mut command, view_root, shell);
    } else {
        push_agent_shell_command(&mut command, shell);
    }
    command
}

#[cfg(target_os = "macos")]
fn macos_agent_viewd_program() -> String {
    std::env::var("AGENT_VIEWD").unwrap_or_else(|_| "agent-viewd".to_string())
}

#[cfg(target_os = "macos")]
fn push_agent_prompt_env(command: &mut Vec<String>, env_id: &str, shell: &str) {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    if shell_name.contains("zsh") {
        command.push(format!("PROMPT={}", zsh_agent_prompt(env_id)));
    } else {
        command.push(format!("PS1={}", bash_agent_prompt(env_id)));
    }
}

#[cfg(target_os = "macos")]
fn push_agent_shell_command(command: &mut Vec<String>, shell: String) {
    let is_zsh = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&shell)
        .contains("zsh");
    command.push(shell);
    if is_zsh {
        command.push("-f".to_string());
    }
}

#[cfg(target_os = "macos")]
fn push_persistent_agent_shell_command(
    command: &mut Vec<String>,
    rootfs_path: &Path,
    shell: String,
) {
    command.push("/bin/sh".to_string());
    command.push("-lc".to_string());
    command.push(persistent_agent_shell_script(rootfs_path, &shell));
}

#[cfg(target_os = "macos")]
fn persistent_agent_shell_script(rootfs_path: &Path, shell: &str) -> String {
    let tmux_dir = rootfs_path.join(".agent-tmux");
    let socket = tmux_dir.join("shell.sock");
    let shell_command = persistent_shell_command(shell);
    let fallback = shell_join(&persistent_shell_argv(shell));
    format!(
        "mkdir -p {tmux_dir}; \
         if command -v tmux >/dev/null 2>&1; then \
           exec tmux -S {socket} new-session -A -s shell {shell_command}; \
         else \
           printf '%s\\n' 'tmux not found; falling back to a non-persistent shell' >&2; \
           exec {fallback}; \
         fi",
        tmux_dir = shell_quote(&tmux_dir.display().to_string()),
        socket = shell_quote(&socket.display().to_string()),
        shell_command = shell_quote(&format!("exec {shell_command}")),
    )
}

#[cfg(target_os = "macos")]
fn persistent_shell_command(shell: &str) -> String {
    shell_join(&persistent_shell_argv(shell))
}

#[cfg(target_os = "macos")]
fn persistent_shell_argv(shell: &str) -> Vec<String> {
    let mut argv = vec![shell.to_string()];
    if shell_is_zsh(shell) {
        argv.push("-f".to_string());
    }
    argv
}

#[cfg(target_os = "macos")]
fn shell_is_zsh(shell: &str) -> bool {
    Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .contains("zsh")
}

#[cfg(target_os = "macos")]
fn zsh_agent_prompt(env_id: &str) -> String {
    format!("%F{{green}}{env_id}%f@%m %1~ %# ")
}

#[cfg(target_os = "macos")]
fn bash_agent_prompt(env_id: &str) -> String {
    format!("\\[\\033[32m\\]{env_id}\\[\\033[0m\\]@\\h \\w \\\\$ ")
}

fn desktop_base_clone_target(
    backend: &RootfsBackend,
    rootfs: &Path,
    from: &Path,
) -> Result<PathBuf> {
    if backend.is_path_preserving_overlay() {
        Ok(rootfs.join(absolute_path_as_overlay_relative(from)?))
    } else {
        Ok(rootfs.to_path_buf())
    }
}

fn clone_desktop_tree(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        reflink::clone_tree_with_copy_fallback(src, dst)
    }
    #[cfg(not(target_os = "windows"))]
    {
        reflink::clone_tree(src, dst)
    }
}

#[cfg(target_os = "macos")]
fn unmount_macos_overlay_if_active(path: &Path) -> Result<()> {
    if !macos_overlay_mount_is_active(path) {
        return Ok(());
    }
    let _ = std::process::Command::new("/sbin/umount")
        .arg(path)
        .status();
    if macos_overlay_mount_is_active(path) {
        let _ = std::process::Command::new("diskutil")
            .arg("unmount")
            .arg("force")
            .arg(path)
            .status();
    }
    if macos_overlay_mount_is_active(path) {
        bail!(
            "failed to unmount active macOS path-preserving overlay at {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn unmount_macos_overlay_if_active(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_overlay_mount_is_active(path: &Path) -> bool {
    let Ok(output) = std::process::Command::new("/sbin/mount").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let mount_point = path.display().to_string();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.contains("agent-overlayfs")
            && (line.contains(&format!(" on {mount_point} ("))
                || line.contains(&format!(" on {} (", mount_point.replace(' ', "\\040"))))
    })
}

#[cfg(windows)]
fn platform_desktop_shell_command(
    rootfs_path: &Path,
    source_root: &Path,
    backend: RootfsBackend,
    env_id: &str,
    host_workspace: Option<&str>,
    limits: &Limits,
    _persistent: bool,
) -> Vec<String> {
    if backend == RootfsBackend::WindowsMinifilterOverlay {
        return windows_minifilter_shell_command(
            rootfs_path,
            source_root,
            env_id,
            host_workspace,
            limits,
        );
    }
    Vec::new()
}

#[cfg(windows)]
fn windows_minifilter_shell_command(
    view_root: &Path,
    source_root: &Path,
    env_id: &str,
    host_workspace: Option<&str>,
    limits: &Limits,
) -> Vec<String> {
    let preserved_cwd = host_workspace.unwrap_or_else(|| source_root.to_str().unwrap_or("\\"));
    let env_dir = view_root.parent().unwrap_or_else(|| Path::new("\\"));
    let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
    vec![
        windows_minifilterctl_program(),
        "exec".to_string(),
        "--env-id".to_string(),
        env_id.to_string(),
        "--source-root".to_string(),
        source_root.display().to_string(),
        "--lower".to_string(),
        env_dir.join("lower").display().to_string(),
        "--upper".to_string(),
        env_dir.join("upper").display().to_string(),
        "--whiteouts".to_string(),
        env_dir.join("whiteouts").display().to_string(),
        "--cwd".to_string(),
        preserved_cwd.to_string(),
        "--network".to_string(),
        limits.network.clone(),
        "--".to_string(),
        shell,
    ]
}

#[cfg(windows)]
fn windows_minifilterctl_program() -> String {
    std::env::var("AGENT_MINIFILTERCTL").unwrap_or_else(|_| "agent-minifilterctl".to_string())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_desktop_shell_command(
    _rootfs_path: &Path,
    _source_root: &Path,
    _backend: RootfsBackend,
    _env_id: &str,
    _host_workspace: Option<&str>,
    _limits: &Limits,
    _persistent: bool,
) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, DesktopService};
    use crate::config::AgentConfig;
    use crate::export::ExportType;
    use crate::model::{
        machine_name, Base, Env, EnvState, LimitOverrides, Limits, NetworkPolicy, RootfsBackend,
        Session, SessionState, SessionType,
    };
    use crate::path_overlay::PathOverlay;
    use crate::protocol::Response;
    use chrono::Utc;
    use std::fs;
    use std::path::Path;

    #[test]
    fn human_bytes_formats_compact_sizes() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(1536), "1.5K");
    }

    #[test]
    fn directory_not_empty_detects_macos_error_code() {
        let error = std::io::Error::from_raw_os_error(66);

        assert!(super::directory_not_empty(&error));
    }

    #[tokio::test]
    async fn remove_dir_all_tolerates_ds_store_residue() {
        let dir = tempfile::tempdir().unwrap();
        let env_dir = dir.path().join("env");
        fs::create_dir_all(&env_dir).unwrap();
        fs::write(env_dir.join(".DS_Store"), "").unwrap();

        super::remove_dir_all_if_exists(&env_dir).await.unwrap();

        assert!(!env_dir.exists());
    }

    #[tokio::test]
    async fn env_destroy_removes_orphan_env_dir_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let env_dir = service.layout.env_dir("orphan");
        fs::create_dir_all(&env_dir).unwrap();
        fs::write(env_dir.join(".DS_Store"), "").unwrap();

        service.env_destroy("orphan").await.unwrap();

        assert!(!env_dir.exists());
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

    #[test]
    fn path_preserving_base_clone_target_uses_absolute_overlay_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("workspace");
        let rootfs = dir.path().join("agentfs/bases/base-001/lower");
        let rel = crate::path_overlay::absolute_path_as_overlay_relative(&source).unwrap();

        assert_eq!(
            super::desktop_base_clone_target(
                &RootfsBackend::PathPreservingOverlay,
                &rootfs,
                &source
            )
            .unwrap(),
            rootfs.join(rel)
        );
    }

    #[tokio::test]
    async fn path_preserving_base_rejects_filesystem_root_source() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));

        let error = service
            .base_freeze_with_backend(
                "base-root",
                Path::new("/"),
                RootfsBackend::PathPreservingOverlay,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("filesystem root"));
    }

    #[tokio::test]
    async fn new_target_reuses_existing_env_before_current_source() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;

        let response = service
            .new_target(
                "codex-1",
                "base-from-root",
                Path::new("/"),
                None,
                "privileged-dev",
                LimitOverrides::default(),
                &[],
                Some(Path::new("/")),
            )
            .await
            .unwrap();

        let Response::DesktopShell { command, .. } = response else {
            panic!("expected desktop shell response");
        };
        #[cfg(target_os = "macos")]
        {
            assert!(command.contains(&"--source-root".to_string()));
            assert!(command.contains(&source.display().to_string()));
        }
        #[cfg(not(target_os = "macos"))]
        assert!(command.is_empty());
        assert!(!service.layout.base_dir("base-from-root").exists());
    }

    #[test]
    fn path_preserving_cwd_outside_source_falls_back_to_source() {
        let cwd = super::path_preserving_cwd_for_backend(
            RootfsBackend::PathPreservingOverlay,
            "/Users/me/project",
            Some(Path::new("/Users/me/other")),
        );

        assert_eq!(cwd, Path::new("/Users/me/project"));
    }

    #[test]
    fn path_preserving_cwd_inside_source_is_preserved() {
        let cwd = super::path_preserving_cwd_for_backend(
            RootfsBackend::PathPreservingOverlay,
            "/Users/me/project",
            Some(Path::new("/Users/me/project/crates/agent-core")),
        );

        assert_eq!(cwd, Path::new("/Users/me/project/crates/agent-core"));
    }

    #[test]
    fn non_path_preserving_cwd_outside_source_is_preserved() {
        let cwd = super::path_preserving_cwd_for_backend(
            RootfsBackend::ApfsClone,
            "/Users/me/project",
            Some(Path::new("/Users/me/other")),
        );

        assert_eq!(cwd, Path::new("/Users/me/other"));
    }

    #[tokio::test]
    async fn desktop_export_reports_rootfs_changed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        create_native_env_fixture(&service).await;

        let changed = service
            .export("codex-1", ExportType::RootfsChangedPaths)
            .await
            .unwrap();

        assert!(changed.contains("/README.md"));
        assert!(changed.contains("/new.txt"));
        assert!(changed.contains("deleted /old.txt"));
        assert_eq!(
            fs::read_to_string(
                service
                    .layout
                    .env_dir("codex-1")
                    .join("exports/rootfs-changed-paths.txt")
            )
            .unwrap(),
            changed
        );
    }

    #[tokio::test]
    async fn desktop_export_rejects_dpkg_delta() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        create_native_env_fixture(&service).await;

        let error = service
            .export("codex-1", ExportType::DpkgDelta)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("dpkg-delta is not implemented"));
    }

    #[tokio::test]
    async fn path_preserving_apply_updates_host_and_clears_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;
        let rel = crate::path_overlay::absolute_path_as_overlay_relative(&source).unwrap();
        let lower_source = service.layout.env_lower("codex-1").join(&rel);
        let upper_source = service.layout.env_upper("codex-1").join(&rel);
        let whiteouts_source = service.layout.env_whiteouts("codex-1").join(&rel);

        let summary = service.apply("codex-1", false).await.unwrap();

        assert!(summary.contains("applied 2 updated, 1 deleted"));
        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("new.txt")).unwrap(),
            "added\n"
        );
        assert!(!source.join("old.txt").exists());
        assert_eq!(
            fs::read_to_string(lower_source.join("README.md")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(lower_source.join("new.txt")).unwrap(),
            "added\n"
        );
        assert!(!lower_source.join("old.txt").exists());
        assert!(!upper_source.join("README.md").exists());
        assert!(!upper_source.join("new.txt").exists());
        assert!(!whiteouts_source.join("old.txt").exists());
        assert_eq!(
            service
                .export("codex-1", ExportType::RootfsChangedPaths)
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn path_preserving_apply_ignores_whiteout_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;
        let rel = crate::path_overlay::absolute_path_as_overlay_relative(&source).unwrap();
        let lower_source = service.layout.env_lower("codex-1").join(&rel);
        let whiteouts_source = service.layout.env_whiteouts("codex-1").join(&rel);
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(source.join(".git/config"), "host git\n").unwrap();
        fs::create_dir_all(lower_source.join(".git")).unwrap();
        fs::write(lower_source.join(".git/config"), "host git\n").unwrap();
        fs::create_dir_all(whiteouts_source.join(".git")).unwrap();

        let summary = service.apply("codex-1", false).await.unwrap();

        assert!(summary.contains("applied 2 updated, 1 deleted"));
        assert_eq!(
            fs::read_to_string(source.join(".git/config")).unwrap(),
            "host git\n"
        );
        assert_eq!(
            fs::read_to_string(lower_source.join(".git/config")).unwrap(),
            "host git\n"
        );
        assert!(!whiteouts_source.join(".git").exists());
    }

    #[tokio::test]
    async fn path_preserving_apply_honors_directory_whiteout_markers() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;
        let rel = crate::path_overlay::absolute_path_as_overlay_relative(&source).unwrap();
        let lower = service.layout.env_lower("codex-1");
        let upper = service.layout.env_upper("codex-1");
        let whiteouts = service.layout.env_whiteouts("codex-1");
        let lower_source = lower.join(&rel);
        let whiteouts_source = whiteouts.join(&rel);
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(source.join(".git/config"), "host git\n").unwrap();
        fs::create_dir_all(lower_source.join(".git")).unwrap();
        fs::write(lower_source.join(".git/config"), "host git\n").unwrap();
        let overlay = PathOverlay::new(lower, upper, whiteouts);
        overlay
            .create_whiteout(&source.join(".git/index.lock"))
            .unwrap();
        overlay.create_whiteout(&source.join(".git")).unwrap();

        let summary = service.apply("codex-1", false).await.unwrap();

        assert!(summary.contains("applied 2 updated, 3 deleted"));
        assert!(!source.join(".git").exists());
        assert!(!lower_source.join(".git").exists());
        assert!(!whiteouts_source.join(".git").exists());
    }

    #[tokio::test]
    async fn path_preserving_apply_rejects_host_conflicts_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;
        fs::write(source.join("README.md"), "host edit\n").unwrap();

        let error = service
            .apply("codex-1", false)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("changed since env creation"));
        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "host edit\n"
        );
        assert_eq!(
            fs::read_to_string(
                service
                    .layout
                    .env_upper("codex-1")
                    .join(crate::path_overlay::absolute_path_as_overlay_relative(&source).unwrap())
                    .join("README.md")
            )
            .unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn path_preserving_apply_force_overwrites_host_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        let source = dir.path().join("workspace");
        create_path_preserving_env_fixture(&service, &source).await;
        fs::write(source.join("README.md"), "host edit\n").unwrap();

        service.apply("codex-1", true).await.unwrap();

        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn desktop_sessions_track_logs_and_state() {
        #[cfg(target_os = "macos")]
        if !macos_sandbox_exec_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        create_native_env_fixture(&service).await;

        service
            .session_create(
                "codex-1",
                "dev",
                &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo hello from desktop session; sleep 30".to_string(),
                ],
                None,
            )
            .await
            .unwrap();

        let sessions = service.session_list("codex-1").await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "dev");
        assert_eq!(sessions[0].state, crate::model::SessionState::Running);

        let logs = service.session_logs("codex-1", "dev").await.unwrap();
        assert!(logs.contains("created native desktop session"));

        service.session_kill("codex-1", "dev").await.unwrap();
        let sessions = service.session_list("codex-1").await.unwrap();
        assert_eq!(sessions[0].state, crate::model::SessionState::Stopped);
    }

    #[cfg(target_os = "macos")]
    fn macos_sandbox_exec_available() -> bool {
        std::process::Command::new("sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[test]
    fn descendant_pids_from_ps_collects_only_the_requested_tree() {
        let ps = "\
              1       0\n\
             10       1\n\
             11      10\n\
             12      10\n\
             13      12\n\
             20       1\n\
             21      20\n";
        let mut descendants = super::descendant_pids_from_ps(ps, 10);
        descendants.sort_unstable();
        assert_eq!(descendants, vec![11, 12, 13]);
    }

    #[tokio::test]
    async fn desktop_session_kill_refuses_pid_one() {
        let dir = tempfile::tempdir().unwrap();
        let service = DesktopService::new(AgentConfig::new(dir.path().join("agentfs")));
        service.init().await.unwrap();
        create_native_env_fixture(&service).await;

        let env_id = "codex-1";
        let session_id = "dev";
        let log_path = super::desktop_session_log_path(&service.layout, env_id, session_id);
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        service
            .layout
            .write_session(&Session {
                id: session_id.to_string(),
                env_id: env_id.to_string(),
                command: "sleep 30".to_string(),
                state: SessionState::Running,
                created_at: Utc::now(),
                session_type: SessionType::Pty,
                log_path,
            })
            .await
            .unwrap();
        fs::write(
            super::desktop_session_pid_path(&service.layout, env_id, session_id),
            "1\n",
        )
        .unwrap();

        let error = service
            .session_kill(env_id, session_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe native desktop session pid 1"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_desktop_shell_uses_client_default_shell() {
        let command = super::desktop_shell_command(
            Path::new("/agentfs/envs/codex-1/rootfs"),
            Path::new("/Users/mizuame/Desktop/project"),
            RootfsBackend::ApfsClone,
            "codex-1",
            None,
            &Limits::default(),
            false,
        );

        assert!(command.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_shell_sets_env_inside_sandbox_command() {
        let rootfs = Path::new("/agentfs/envs/codex-1/rootfs");
        let command = super::desktop_shell_command(
            rootfs,
            Path::new("/Users/mizuame/Desktop/project"),
            RootfsBackend::ApfsClone,
            "codex-1",
            Some("/Users/mizuame/Desktop/project"),
            &Limits::default(),
            false,
        );

        assert_eq!(command[0], "sandbox-exec");
        assert_eq!(command[3], "/usr/bin/env");
        assert!(command[2].contains("(allow network*)"));
        assert!(command.contains(&"HOME=/agentfs/envs/codex-1/rootfs".to_string()));
        assert!(command.contains(&"ZDOTDIR=/agentfs/envs/codex-1/rootfs".to_string()));
        assert!(command.contains(&"TMPDIR=/agentfs/envs/codex-1/rootfs/.tmp".to_string()));
        assert!(command.contains(&"AGENT_ENV_ID=codex-1".to_string()));
        assert!(command.contains(&"AGENT_NETWORK=host".to_string()));
        let has_zsh_prompt = command
            .iter()
            .any(|arg| arg.starts_with("PROMPT=%F{green}codex-1%f@%m"));
        let has_bash_prompt = command
            .iter()
            .any(|arg| arg.starts_with("PS1=\\[\\033[32m\\]codex-1"));
        assert!(has_zsh_prompt || has_bash_prompt);
        assert!(command.contains(&"HOST_WORKSPACE=/Users/mizuame/Desktop/project".to_string()));
        assert!(!command.iter().any(|arg| arg.contains("tmux -S")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_persistent_desktop_shell_uses_tmux_socket() {
        let rootfs = Path::new("/agentfs/envs/codex-1/rootfs");
        let command = super::desktop_shell_command(
            rootfs,
            Path::new("/Users/mizuame/Desktop/project"),
            RootfsBackend::ApfsClone,
            "codex-1",
            Some("/Users/mizuame/Desktop/project"),
            &Limits::default(),
            true,
        );

        let script = command.last().expect("shell wrapper script");
        assert!(script.contains("tmux -S"));
        assert!(script.contains("/agentfs/envs/codex-1/rootfs/.agent-tmux/shell.sock"));
        assert!(script.contains("new-session -A -s shell"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_shell_can_disable_network() {
        let rootfs = Path::new("/agentfs/envs/codex-1/rootfs");
        let mut limits = Limits::default();
        limits.network = "none".to_string();
        let command = super::desktop_shell_command(
            rootfs,
            Path::new("/Users/mizuame/Desktop/project"),
            RootfsBackend::ApfsClone,
            "codex-1",
            None,
            &limits,
            false,
        );

        assert!(command[2].contains("(deny network*)"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_preserving_shell_uses_view_helper_with_preserved_cwd() {
        let view_root = Path::new("/Users/mizuame/.agentfs/envs/codex-1/view-root");
        let command = super::desktop_shell_command(
            view_root,
            Path::new("/Users/mizuame/Desktop/script/example"),
            RootfsBackend::PathPreservingOverlay,
            "codex-1",
            Some("/Users/mizuame/Desktop/script/example"),
            &Limits::default(),
            false,
        );

        assert_eq!(command[0], "agent-viewd");
        assert!(command.contains(&"shell".to_string()));
        assert!(command.contains(&"--view-root".to_string()));
        assert!(command.contains(&view_root.display().to_string()));
        assert!(command.contains(&"--source-root".to_string()));
        assert!(command.contains(&"/Users/mizuame/Desktop/script/example".to_string()));
        assert!(command.contains(&"--cwd".to_string()));
        assert!(command.contains(&"/Users/mizuame/Desktop/script/example".to_string()));
        assert!(!command.contains(&"sandbox-exec".to_string()));
        assert!(!command
            .iter()
            .any(|arg| arg == "HOME=/Users/mizuame/Desktop/script/example"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_shell_runs_zsh_without_rc_prompt_override() {
        let mut command = Vec::new();

        super::push_agent_shell_command(&mut command, "/bin/zsh".to_string());
        super::push_agent_shell_command(&mut command, "/bin/bash".to_string());

        assert_eq!(command, vec!["/bin/zsh", "-f", "/bin/bash"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_persistent_shell_falls_back_without_tmux() {
        let script = super::persistent_agent_shell_script(
            Path::new("/agentfs/envs/codex-1/rootfs"),
            "/bin/zsh",
        );

        assert!(script.contains("tmux not found"));
        assert!(script.contains("exec /bin/zsh -f"));
    }

    async fn create_native_env_fixture(service: &DesktopService) {
        let base_rootfs = service.layout.base_rootfs("base-001");
        let env_rootfs = service.layout.env_rootfs("codex-1");
        fs::create_dir_all(&base_rootfs).unwrap();
        fs::create_dir_all(&env_rootfs).unwrap();
        fs::create_dir_all(service.layout.env_dir("codex-1").join("exports")).unwrap();
        fs::write(base_rootfs.join("README.md"), "old\n").unwrap();
        fs::write(base_rootfs.join("old.txt"), "deleted\n").unwrap();
        fs::write(env_rootfs.join("README.md"), "new\n").unwrap();
        fs::write(env_rootfs.join("new.txt"), "added\n").unwrap();

        let created_at = Utc::now();
        service
            .layout
            .write_base(&Base {
                id: "base-001".to_string(),
                backend: RootfsBackend::ApfsClone,
                rootfs_path: base_rootfs,
                readonly: true,
                created_at,
                source: "fixture".to_string(),
                dpkg_manifest: service.layout.base_dir("base-001").join("dpkg.list"),
            })
            .await
            .unwrap();
        service
            .layout
            .write_env(&Env {
                id: "codex-1".to_string(),
                base_id: "base-001".to_string(),
                backend: RootfsBackend::ApfsClone,
                rootfs_path: env_rootfs,
                machine_name: machine_name("codex-1"),
                state: EnvState::Running,
                profile: "default".to_string(),
                created_at,
                last_active_at: created_at,
                limits: Limits::default(),
                network_policy: NetworkPolicy::default(),
                sessions: Vec::new(),
            })
            .await
            .unwrap();
    }

    async fn create_path_preserving_env_fixture(service: &DesktopService, source: &Path) {
        fs::create_dir_all(source).unwrap();
        fs::write(source.join("README.md"), "old\n").unwrap();
        fs::write(source.join("old.txt"), "deleted\n").unwrap();
        let rel = crate::path_overlay::absolute_path_as_overlay_relative(source).unwrap();
        let base_lower = service.layout.base_lower("base-pp");
        let env_lower = service.layout.env_lower("codex-1");
        let env_upper = service.layout.env_upper("codex-1");
        let env_whiteouts = service.layout.env_whiteouts("codex-1");
        let lower_source = env_lower.join(&rel);
        let upper_source = env_upper.join(&rel);
        let whiteouts_source = env_whiteouts.join(&rel);
        fs::create_dir_all(&base_lower).unwrap();
        fs::create_dir_all(&lower_source).unwrap();
        fs::create_dir_all(&upper_source).unwrap();
        fs::create_dir_all(&whiteouts_source).unwrap();
        fs::create_dir_all(service.layout.env_dir("codex-1").join("exports")).unwrap();
        fs::create_dir_all(service.layout.session_logs("codex-1")).unwrap();
        fs::write(lower_source.join("README.md"), "old\n").unwrap();
        fs::write(lower_source.join("old.txt"), "deleted\n").unwrap();
        fs::write(upper_source.join("README.md"), "new\n").unwrap();
        fs::write(upper_source.join("new.txt"), "added\n").unwrap();
        fs::write(whiteouts_source.join("old.txt"), "").unwrap();

        let created_at = Utc::now();
        service
            .layout
            .write_base(&Base {
                id: "base-pp".to_string(),
                backend: RootfsBackend::PathPreservingOverlay,
                rootfs_path: base_lower,
                readonly: true,
                created_at,
                source: source.display().to_string(),
                dpkg_manifest: service.layout.base_dir("base-pp").join("dpkg.list"),
            })
            .await
            .unwrap();
        service
            .layout
            .write_env(&Env {
                id: "codex-1".to_string(),
                base_id: "base-pp".to_string(),
                backend: RootfsBackend::PathPreservingOverlay,
                rootfs_path: service.layout.env_view_root("codex-1"),
                machine_name: machine_name("codex-1"),
                state: EnvState::Running,
                profile: "default".to_string(),
                created_at,
                last_active_at: created_at,
                limits: Limits::default(),
                network_policy: NetworkPolicy::default(),
                sessions: Vec::new(),
            })
            .await
            .unwrap();
    }
}
