use crate::btrfs::Btrfs;
use crate::command::{shell_join, CommandRunner};
use crate::config::AgentConfig;
use crate::export::{ExportType, Exporter};
use crate::model::{
    machine_name, Base, Env, EnvState, EnvStatus, LimitOverrides, Session, SessionState,
};
use crate::nspawn::Nspawn;
use crate::protocol::{Request, Response};
use crate::session::TmuxSessionBackend;
use crate::storage::{validate_id, write_text_file, Layout};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
            Request::BaseFreeze { name, from } => {
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
            Request::Exec { id, command } => {
                let output = self.exec(&id, &command).await?;
                Ok(Response::Exec {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Request::Shell { id } => {
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
            Request::Ping => Ok(Response::Ok),
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.btrfs.ensure_filesystem(&self.config.agentfs).await?;
        self.layout.ensure_agentfs().await?;
        self.btrfs.enable_quota(&self.config.agentfs).await?;
        Ok(())
    }

    pub async fn base_freeze(&self, name: &str, from: &Path) -> Result<Base> {
        validate_id(name)?;
        self.btrfs.ensure_subvolume(from).await?;
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
        if let Err(error) =
            write_text_file(&base_dir.join("created_at"), Utc::now().to_rfc3339()).await
        {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
            return Err(error);
        }
        let base = Base {
            id: name.to_string(),
            rootfs_path: rootfs.clone(),
            readonly: true,
            created_at: Utc::now(),
            source: from.display().to_string(),
            dpkg_manifest,
        };
        if let Err(error) = self.layout.write_base(&base).await {
            self.cleanup_failed_base_freeze(&rootfs, &base_dir).await;
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
        tokio::fs::create_dir_all(&env_dir).await?;
        let rootfs = self.layout.env_rootfs(id);
        if let Err(error) = self
            .btrfs
            .snapshot_writable(&base.rootfs_path, &rootfs)
            .await
        {
            cleanup_failed_env_dir(&env_dir).await;
            return Err(error);
        }
        if let Err(error) = self.btrfs.set_limit(&limits.disk_max, &rootfs).await {
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        if let Err(error) = self.ensure_env_dirs(id).await {
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        let env = Env {
            id: id.to_string(),
            base_id: base_id.to_string(),
            rootfs_path: rootfs.clone(),
            machine_name: machine_name(id),
            state: EnvState::Created,
            profile: profile.name.clone(),
            created_at: Utc::now(),
            limits,
            sessions: Vec::new(),
        };
        if let Err(error) = self.log_daemon(id, "env created").await {
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        if let Err(error) = self.log_lifecycle(id, "created").await {
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        if let Err(error) = self.nspawn.write_config(&env).await {
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        if let Err(error) = self.layout.write_env(&env).await {
            let _ = self.nspawn.remove_config(&env).await;
            self.cleanup_failed_env_create(&rootfs, &env_dir).await;
            return Err(error);
        }
        Ok(env)
    }

    pub async fn env_start(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        let nspawn_log = self.layout.nspawn_log(id);
        self.log_lifecycle(id, "starting").await?;
        if let Err(error) = validate_child_rootfs_requirements(&env.rootfs_path) {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_lifecycle(id, &format!("failed preflight: {error:#}"))
                .await?;
            return Err(error);
        }
        if let Err(error) = self.nspawn.start(&env, Some(&nspawn_log)).await {
            env.state = EnvState::Failed;
            self.layout.write_env(&env).await?;
            self.log_lifecycle(id, &format!("failed start: {error:#}"))
                .await?;
            return Err(error);
        }
        env.state = EnvState::Running;
        self.log_lifecycle(id, "running").await?;
        self.layout.write_env(&env).await?;
        Ok(())
    }

    pub async fn env_stop(&self, id: &str) -> Result<()> {
        let mut env = self.layout.read_env(id).await?;
        self.log_lifecycle(id, "stopping").await?;
        self.nspawn.stop(&env.machine_name).await?;
        if should_mark_stopped(&env.state) {
            env.state = EnvState::Stopped;
        }
        self.log_lifecycle(id, "stopped").await?;
        self.layout.write_env(&env).await?;
        Ok(())
    }

    pub async fn env_destroy(&self, id: &str) -> Result<()> {
        let env = self.layout.read_env(id).await?;
        self.log_lifecycle(id, "destroying").await?;
        self.nspawn.stop(&env.machine_name).await?;
        let qgroup_id = self.btrfs.qgroup_id(&env.rootfs_path).await?;
        self.btrfs.delete_subvolume(&env.rootfs_path).await?;
        if let Some(qgroup_id) = qgroup_id {
            self.btrfs
                .destroy_qgroup(&qgroup_id, &self.config.agentfs)
                .await?;
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
        if should_check_quota(&env.state) && self.btrfs.quota_exceeded(&env.rootfs_path).await? {
            env.state = EnvState::QuotaExceeded;
            self.log_lifecycle(id, "quota exceeded during status refresh")
                .await?;
        }
        self.layout.write_env(&env).await?;
        let disk_used = self.disk_used(&env.rootfs_path).await.ok();
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
        self.log_lifecycle(id, &format!("exec {}", shell_join(command)))
            .await?;
        let output = self.nspawn.exec(&env, command, &log_path).await?;
        if self.btrfs.quota_exceeded(&env.rootfs_path).await? {
            env.state = EnvState::QuotaExceeded;
            self.layout.write_env(&env).await?;
            self.log_lifecycle(id, "quota exceeded after exec").await?;
        }
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
        let metadata_exists = self.layout.read_session(env_id, session_id).await.is_ok();
        let running = metadata_exists && self.sessions.is_running(&env, session_id).await?;
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
        self.layout.write_session(&session).await?;
        self.layout.write_env(&env).await?;
        self.log_lifecycle(env_id, &format!("session {session_id} created"))
            .await?;
        Ok(())
    }

    pub async fn shell_attach_target(&self, env_id: &str) -> Result<(String, String)> {
        let env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let session_id = "shell";
        if self.layout.read_session(env_id, session_id).await.is_err()
            || !self.sessions.is_running(&env, session_id).await?
        {
            self.session_create(env_id, session_id, &["bash".to_string()])
                .await?;
        }
        Ok((env.machine_name, session_id.to_string()))
    }

    pub async fn session_attach_target(&self, env_id: &str, session_id: &str) -> Result<Env> {
        let env = self.layout.read_env(env_id).await?;
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
        Ok(env)
    }

    pub async fn session_logs(&self, env_id: &str, session_id: &str) -> Result<String> {
        let env = self.layout.read_env(env_id).await?;
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
        self.log_lifecycle(env_id, &format!("session {session_id} logs synced"))
            .await?;
        Ok(logs)
    }

    pub async fn session_detach(&self, env_id: &str, session_id: &str) -> Result<()> {
        let env = self.layout.read_env(env_id).await?;
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
        self.log_lifecycle(env_id, &format!("session {session_id} detached"))
            .await?;
        Ok(())
    }

    pub async fn session_kill(&self, env_id: &str, session_id: &str) -> Result<()> {
        let env = self.layout.read_env(env_id).await?;
        ensure_running_env(&env)?;
        let mut session = self.layout.read_session(env_id, session_id).await?;
        self.sessions.kill(&env, &session.id).await?;
        session.state = SessionState::Stopped;
        self.layout.write_session(&session).await?;
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
        let env = self.layout.read_env(env_id).await?;
        self.exporter.workspace_patch(&env).await
    }

    pub async fn export(&self, env_id: &str, export_type: ExportType) -> Result<String> {
        let env = self.layout.read_env(env_id).await?;
        let base = self.layout.read_base(&env.base_id).await?;
        let text = match export_type {
            ExportType::WorkspacePatch => self.exporter.workspace_patch(&env).await,
            ExportType::RootfsChangedPaths => {
                Exporter::changed_paths_by_walk(&base.rootfs_path, &env.rootfs_path)
            }
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
        for rel in [
            "proc",
            "sys",
            "dev",
            "run",
            "tmp",
            "agentfs/bases",
            "agentfs/envs",
            "agentfs/cache",
            "agentfs/runtime",
        ] {
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

    async fn cleanup_failed_env_create(&self, rootfs: &Path, env_dir: &Path) {
        let _ = self.btrfs.delete_subvolume(rootfs).await;
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
        ("bash", &["bin/bash", "usr/bin/bash"][..]),
        ("sudo", &["usr/bin/sudo", "bin/sudo"][..]),
        ("tmux", &["usr/bin/tmux", "bin/tmux"][..]),
        ("apt", &["usr/bin/apt", "usr/bin/apt-get"][..]),
    ] {
        if !candidates
            .iter()
            .any(|candidate| rootfs.join(candidate).exists())
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

#[cfg(test)]
mod tests {
    use super::{
        cleanup_failed_base_dir, cleanup_failed_env_dir, ensure_running_env,
        read_offline_session_log, read_session_log_file, remove_dir_all_if_exists,
        remove_path_if_exists, should_check_quota, should_mark_stopped, should_refresh_live_state,
        should_reject_session_create, sync_env_session_index, validate_child_rootfs_requirements,
        AgentService,
    };
    use crate::config::AgentConfig;
    use crate::model::{machine_name, Env, EnvState, Limits, Session, SessionState, SessionType};
    use chrono::Utc;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rootfs_preflight_requires_sudo_apt_tmux_and_bash() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("usr/bin")).unwrap();
        fs::write(dir.path().join("bin/bash"), "").unwrap();
        fs::write(dir.path().join("usr/bin/sudo"), "").unwrap();
        fs::write(dir.path().join("usr/bin/apt-get"), "").unwrap();
        fs::write(dir.path().join("usr/bin/tmux"), "").unwrap();
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
        ] {
            fs::create_dir_all(dir.path().join(rel)).unwrap();
        }
        fs::write(dir.path().join("agentfs/bases/base-001/secret"), "").unwrap();
        fs::write(dir.path().join("agentfs/envs/sibling/secret"), "").unwrap();

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
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
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
}
