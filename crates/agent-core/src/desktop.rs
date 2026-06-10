use crate::command::{CmdOutput, CommandRunner};
#[cfg(target_os = "macos")]
use crate::command::macos_sandbox_profile;
use crate::config::AgentConfig;
use crate::export::{ExportType, Exporter};
use crate::model::{
    machine_name, Base, Env, EnvState, EnvStatus, LimitOverrides, RootfsBackend, Session,
    SessionState, SessionType,
};
use crate::protocol::{Request, Response};
use crate::reflink;
use crate::storage::{validate_id, write_text_file, Layout};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DesktopService {
    pub config: AgentConfig,
    layout: Layout,
    exporter: Exporter,
    runner: CommandRunner,
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
                profile,
                limits,
                command,
            } => {
                self.new_target(&target, &base, &from, &profile, limits, &command)
                    .await
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
                let env = self.shell_target(&id).await?;
                Ok(Response::DesktopShell {
                    command: desktop_shell_command(&env.rootfs_path),
                    rootfs_path: env.rootfs_path,
                })
            }
            Request::Ping => Ok(Response::Ok),
            Request::SessionCreate {
                env_id,
                session_id,
                command,
            } => {
                self.session_create(&env_id, &session_id, &command).await?;
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
        }
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
            let env = self.shell_target(target).await?;
            Ok(Response::DesktopShell {
                command: desktop_shell_command(&env.rootfs_path),
                rootfs_path: env.rootfs_path,
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

    pub async fn env_list(&self) -> Result<Vec<EnvStatus>> {
        let envs = self.layout.list_envs().await?;
        let mut statuses = Vec::with_capacity(envs.len());
        for env in envs {
            statuses.push(self.env_status(&env.id).await?);
        }
        Ok(statuses)
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
            .run_desktop_isolated(&env.rootfs_path, program, args, &env.limits)
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
        let pid = self.runner.spawn_desktop_session(
            &env.rootfs_path,
            program,
            args,
            &log_path,
            &env.limits,
        )?;
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
                Exporter::changed_paths_by_walk(&base.rootfs_path, &env.rootfs_path)?
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

    async fn shell_target(&self, id: &str) -> Result<Env> {
        let env = self.layout.read_env(id).await?;
        ensure_desktop_backend(&env)?;
        if env.state != EnvState::Running {
            return Err(anyhow!("env {id} is not running"));
        }
        Ok(env)
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

#[cfg(unix)]
async fn process_running(runner: &CommandRunner, pid: u32) -> Result<bool> {
    let output = runner
        .run("kill", vec!["-0".to_string(), pid.to_string()])
        .await?;
    Ok(output.status == 0)
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
    Ok(output.status == 0
        && output
            .stdout
            .split_whitespace()
            .any(|field| field == pid.to_string()))
}

#[cfg(unix)]
async fn kill_process_tree(runner: &CommandRunner, pid: u32) -> Result<()> {
    let output = runner.run("kill", vec![pid.to_string()]).await?;
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

#[cfg(windows)]
async fn kill_process_tree(runner: &CommandRunner, pid: u32) -> Result<()> {
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

fn desktop_shell_command(rootfs_path: &Path) -> Vec<String> {
    platform_desktop_shell_command(rootfs_path)
}

#[cfg(target_os = "macos")]
fn platform_desktop_shell_command(rootfs_path: &Path) -> Vec<String> {
    vec![
        "sandbox-exec".to_string(),
        "-p".to_string(),
        macos_sandbox_profile(rootfs_path),
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
    ]
}

#[cfg(not(target_os = "macos"))]
fn platform_desktop_shell_command(_rootfs_path: &Path) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, DesktopService};
    use crate::config::AgentConfig;
    use crate::export::ExportType;
    use crate::model::{machine_name, Base, Env, EnvState, Limits, NetworkPolicy, RootfsBackend};
    use chrono::Utc;
    use std::fs;
    use std::path::Path;

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
    async fn desktop_sessions_track_logs_and_state() {
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_desktop_shell_uses_client_default_shell() {
        let command = super::desktop_shell_command(Path::new("/agentfs/envs/codex-1/rootfs"));

        assert!(command.is_empty());
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
}
