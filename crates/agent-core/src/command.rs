use crate::model::Limits;
use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Default)]
pub struct CommandRunner;

impl CommandRunner {
    pub async fn run<I, S>(&self, program: &str, args: I) -> Result<CmdOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new(program)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to execute {program}"))?;
        Ok(CmdOutput {
            status: output.status.code().unwrap_or(128),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    pub async fn run_checked<I, S>(&self, program: &str, args: I) -> Result<CmdOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(program, args).await?;
        if output.status != 0 {
            return Err(anyhow!(
                "{program} exited with {}: {}{}",
                output.status,
                output.stdout,
                output.stderr
            ));
        }
        Ok(output)
    }

    pub async fn run_in_dir<I, S>(&self, cwd: &Path, program: &str, args: I) -> Result<CmdOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to execute {program} in {}", cwd.display()))?;
        Ok(CmdOutput {
            status: output.status.code().unwrap_or(128),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    pub async fn run_desktop_isolated(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
        limits: &Limits,
    ) -> Result<CmdOutput> {
        run_desktop_isolated(cwd, program, args, limits).await
    }

    pub fn spawn_desktop_session(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
        log_path: &Path,
        limits: &Limits,
    ) -> Result<u32> {
        spawn_desktop_session(cwd, program, args, log_path, limits)
    }

    pub async fn run_macos_path_preserving_overlay(
        &self,
        view_root: &Path,
        lower: &Path,
        upper: &Path,
        whiteouts: &Path,
        preserved_cwd: &Path,
        program: &str,
        args: &[String],
        limits: &Limits,
    ) -> Result<CmdOutput> {
        run_macos_path_preserving_overlay(
            view_root,
            lower,
            upper,
            whiteouts,
            preserved_cwd,
            program,
            args,
            limits,
        )
        .await
    }

    pub fn spawn_macos_path_preserving_overlay_session(
        &self,
        view_root: &Path,
        lower: &Path,
        upper: &Path,
        whiteouts: &Path,
        preserved_cwd: &Path,
        program: &str,
        args: &[String],
        log_path: &Path,
        limits: &Limits,
    ) -> Result<u32> {
        spawn_macos_path_preserving_overlay_session(
            view_root,
            lower,
            upper,
            whiteouts,
            preserved_cwd,
            program,
            args,
            log_path,
            limits,
        )
    }

    pub async fn append_to_file(path: &Path, content: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        if let Some(parent) = path.parent() {
            sync_parent_dir(parent)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
async fn run_macos_path_preserving_overlay(
    view_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
    preserved_cwd: &Path,
    program: &str,
    args: &[String],
    limits: &Limits,
) -> Result<CmdOutput> {
    let mut command = Command::new(macos_agent_viewd_program());
    command
        .arg("exec")
        .arg("--view-root")
        .arg(view_root)
        .arg("--lower")
        .arg(lower)
        .arg("--upper")
        .arg(upper)
        .arg("--whiteouts")
        .arg(whiteouts)
        .arg("--cwd")
        .arg(preserved_cwd)
        .arg("--network")
        .arg(&limits.network)
        .arg("--")
        .arg(program)
        .args(args);
    run_macos_viewd_command(command)
        .await
        .with_context(|| "failed to execute agent-viewd for macOS path-preserving overlay")
}

#[cfg(not(target_os = "macos"))]
async fn run_macos_path_preserving_overlay(
    _view_root: &Path,
    _lower: &Path,
    _upper: &Path,
    _whiteouts: &Path,
    _preserved_cwd: &Path,
    _program: &str,
    _args: &[String],
    _limits: &Limits,
) -> Result<CmdOutput> {
    Err(anyhow!(
        "macOS path-preserving overlay execution is available only on macOS"
    ))
}

#[cfg(target_os = "macos")]
fn spawn_macos_path_preserving_overlay_session(
    view_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
    preserved_cwd: &Path,
    program: &str,
    args: &[String],
    log_path: &Path,
    limits: &Limits,
) -> Result<u32> {
    let mut command = std::process::Command::new(macos_agent_viewd_program());
    command
        .arg("session")
        .arg("--view-root")
        .arg(view_root)
        .arg("--lower")
        .arg(lower)
        .arg("--upper")
        .arg(upper)
        .arg("--whiteouts")
        .arg(whiteouts)
        .arg("--cwd")
        .arg(preserved_cwd)
        .arg("--network")
        .arg(&limits.network)
        .arg("--log-path")
        .arg(log_path)
        .arg("--")
        .arg(program)
        .args(args);
    let output = run_macos_viewd_command_blocking(&mut command).with_context(|| {
        "failed to execute agent-viewd session for macOS path-preserving overlay"
    })?;
    if output.status != 0 {
        return Err(anyhow!(
            "agent-viewd session exited with {}: {}{}",
            output.status,
            output.stdout,
            output.stderr
        ));
    }
    output.stdout.trim().parse::<u32>().with_context(|| {
        format!(
            "agent-viewd session returned invalid pid: {:?}",
            output.stdout
        )
    })
}

#[cfg(target_os = "macos")]
fn macos_agent_viewd_program() -> PathBuf {
    std::env::var_os("AGENT_VIEWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("agent-viewd"))
}

#[cfg(target_os = "macos")]
async fn run_macos_viewd_command(mut command: Command) -> Result<CmdOutput> {
    let capture = CaptureFiles::create("agent-viewd")?;
    command
        .stdout(Stdio::from(capture.stdout_file.try_clone()?))
        .stderr(Stdio::from(capture.stderr_file.try_clone()?));
    let status = command.spawn()?.wait().await?;
    capture.output(status.code().unwrap_or(128))
}

#[cfg(target_os = "macos")]
fn run_macos_viewd_command_blocking(command: &mut std::process::Command) -> Result<CmdOutput> {
    let capture = CaptureFiles::create("agent-viewd-session")?;
    command
        .stdout(Stdio::from(capture.stdout_file.try_clone()?))
        .stderr(Stdio::from(capture.stderr_file.try_clone()?));
    let status = command.spawn()?.wait()?;
    capture.output(status.code().unwrap_or(128))
}

#[cfg(target_os = "macos")]
struct CaptureFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_file: std::fs::File,
    stderr_file: std::fs::File,
}

#[cfg(target_os = "macos")]
impl CaptureFiles {
    fn create(label: &str) -> Result<Self> {
        let mut last_error = None;
        for attempt in 0..32 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let base = std::env::temp_dir().join(format!(
                "ipa-rs-{label}-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            let stdout_path = base.with_extension("stdout");
            let stderr_path = base.with_extension("stderr");
            match (
                create_capture_file(&stdout_path),
                create_capture_file(&stderr_path),
            ) {
                (Ok(stdout_file), Ok(stderr_file)) => {
                    return Ok(Self {
                        stdout_path,
                        stderr_path,
                        stdout_file,
                        stderr_file,
                    });
                }
                (stdout_result, stderr_result) => {
                    let _ = std::fs::remove_file(&stdout_path);
                    let _ = std::fs::remove_file(&stderr_path);
                    last_error = stdout_result.err().or_else(|| stderr_result.err());
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| anyhow!("failed to create temporary capture files for {label}")))
    }

    fn output(self, status: i32) -> Result<CmdOutput> {
        let stdout = std::fs::read_to_string(&self.stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
        Ok(CmdOutput {
            status,
            stdout,
            stderr,
        })
    }
}

#[cfg(target_os = "macos")]
impl Drop for CaptureFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

#[cfg(target_os = "macos")]
fn create_capture_file(path: &Path) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?)
}

#[cfg(not(target_os = "macos"))]
fn spawn_macos_path_preserving_overlay_session(
    _view_root: &Path,
    _lower: &Path,
    _upper: &Path,
    _whiteouts: &Path,
    _preserved_cwd: &Path,
    _program: &str,
    _args: &[String],
    _log_path: &Path,
    _limits: &Limits,
) -> Result<u32> {
    Err(anyhow!(
        "macOS path-preserving overlay sessions are available only on macOS"
    ))
}

#[cfg(not(windows))]
fn sync_parent_dir(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .with_context(|| format!("failed to open log dir {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync log dir {}", parent.display()))?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent_dir(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
async fn run_desktop_isolated(
    cwd: &Path,
    program: &str,
    args: &[String],
    limits: &Limits,
) -> Result<CmdOutput> {
    let profile = macos_sandbox_profile(cwd, &limits.network);
    let mut command = Command::new("sandbox-exec");
    apply_macos_desktop_env(cwd, &mut command, &limits.network)?;
    command
        .current_dir(cwd)
        .arg("-p")
        .arg(profile)
        .arg(program)
        .args(args);
    let output = command
        .output()
        .await
        .with_context(|| "failed to execute sandbox-exec for native desktop env")?;
    Ok(CmdOutput {
        status: output.status.code().unwrap_or(128),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_sandbox_profile(rootfs: &Path, network: &str) -> String {
    let rootfs = scheme_string(rootfs.display().to_string().as_str());
    let network_rule = macos_network_sandbox_rule(network);
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow file-read*)
(allow file-ioctl)
(allow file-write* (subpath "{rootfs}"))
{network_rule}
"#
    )
}

#[cfg(target_os = "macos")]
fn macos_network_sandbox_rule(network: &str) -> &'static str {
    match network {
        "host" | "bridge" => "(allow network*)",
        "none" => "(deny network*)",
        _ => "(deny network*)",
    }
}

#[cfg(target_os = "macos")]
fn spawn_desktop_session(
    cwd: &Path,
    program: &str,
    args: &[String],
    log_path: &Path,
    limits: &Limits,
) -> Result<u32> {
    let mut command = Command::new("sandbox-exec");
    apply_macos_desktop_env(cwd, &mut command, &limits.network)?;
    command
        .current_dir(cwd)
        .arg("-p")
        .arg(macos_sandbox_profile(cwd, &limits.network))
        .arg(program)
        .args(args);
    spawn_logged(command, log_path)
}

#[cfg(target_os = "macos")]
fn scheme_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn apply_macos_desktop_env(cwd: &Path, command: &mut Command, network: &str) -> Result<()> {
    let tmpdir = cwd.join(".tmp");
    std::fs::create_dir_all(&tmpdir)
        .with_context(|| format!("failed to create desktop tmpdir {}", tmpdir.display()))?;
    command.env("HOME", cwd);
    command.env("ZDOTDIR", cwd);
    command.env("TMPDIR", tmpdir);
    command.env("AGENT_NETWORK", network);
    if let Ok(host_home) = std::env::var("HOME") {
        command.env("HOST_HOME", host_home);
    }
    Ok(())
}

#[cfg(windows)]
async fn run_desktop_isolated(
    cwd: &Path,
    program: &str,
    args: &[String],
    limits: &Limits,
) -> Result<CmdOutput> {
    windows_job::run_in_job(
        cwd.to_path_buf(),
        program.to_string(),
        args.to_vec(),
        limits.clone(),
    )
    .await
}

#[cfg(windows)]
fn spawn_desktop_session(
    cwd: &Path,
    program: &str,
    args: &[String],
    log_path: &Path,
    limits: &Limits,
) -> Result<u32> {
    windows_job::spawn_logged_in_job(
        cwd.to_path_buf(),
        program.to_string(),
        args.to_vec(),
        log_path,
        limits.clone(),
    )
}

#[cfg(windows)]
pub(crate) fn terminate_desktop_session_job(pid: u32) -> Result<bool> {
    windows_job::terminate_session_job(pid)
}

#[cfg(windows)]
pub(crate) fn forget_desktop_session_job(pid: u32) {
    windows_job::forget_session_job(pid);
}

#[cfg(not(any(target_os = "macos", windows)))]
async fn run_desktop_isolated(
    cwd: &Path,
    program: &str,
    args: &[String],
    _limits: &Limits,
) -> Result<CmdOutput> {
    CommandRunner.run_in_dir(cwd, program, args).await
}

#[cfg(not(any(target_os = "macos", windows)))]
fn spawn_desktop_session(
    cwd: &Path,
    program: &str,
    args: &[String],
    log_path: &Path,
    _limits: &Limits,
) -> Result<u32> {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    spawn_logged(command, log_path)
}

#[cfg(not(windows))]
fn spawn_logged(mut command: Command, log_path: &Path) -> Result<u32> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create session log dir {}", parent.display()))?;
    }
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open session log {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to clone session log {}", log_path.display()))?;
    #[cfg(unix)]
    command.process_group(0);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn native desktop session")?;
    child
        .id()
        .ok_or_else(|| anyhow!("spawned native desktop session without a process id"))
}

#[cfg(windows)]
mod windows_job {
    use super::CmdOutput;
    use crate::model::Limits;
    use anyhow::{anyhow, Context, Result};
    use std::collections::HashMap;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::{Command as StdCommand, Stdio};
    use std::sync::{Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub async fn run_in_job(
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        limits: Limits,
    ) -> Result<CmdOutput> {
        tokio::task::spawn_blocking(move || run_in_job_blocking(cwd, program, args, limits))
            .await
            .context("native desktop job task panicked")?
    }

    fn run_in_job_blocking(
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        limits: Limits,
    ) -> Result<CmdOutput> {
        let job = Job::create()?;
        job.apply_limits(limits.pids_max)?;

        let child = StdCommand::new(&program)
            .current_dir(&cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute {program} in {}", cwd.display()))?;

        job.assign(child.as_raw_handle() as HANDLE)?;
        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for {program}"))?;
        Ok(CmdOutput {
            status: output.status.code().unwrap_or(128),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    pub fn spawn_logged_in_job(
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        log_path: &Path,
        limits: Limits,
    ) -> Result<u32> {
        let job = Job::create()?;
        job.apply_limits(limits.pids_max)?;

        let stdout = open_session_log(log_path)?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("failed to clone session log {}", log_path.display()))?;
        let child = StdCommand::new(&program)
            .current_dir(&cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn native desktop session in {}",
                    cwd.display()
                )
            })?;
        let pid = child.id();
        job.assign(child.as_raw_handle() as HANDLE)?;
        session_jobs()
            .lock()
            .map_err(|_| anyhow!("Windows session job registry is poisoned"))?
            .insert(pid, job);
        Ok(pid)
    }

    pub fn terminate_session_job(pid: u32) -> Result<bool> {
        let Some(job) = session_jobs()
            .lock()
            .map_err(|_| anyhow!("Windows session job registry is poisoned"))?
            .remove(&pid)
        else {
            return Ok(false);
        };
        job.terminate(1)?;
        Ok(true)
    }

    pub fn forget_session_job(pid: u32) {
        if let Ok(mut jobs) = session_jobs().lock() {
            jobs.remove(&pid);
        }
    }

    fn open_session_log(log_path: &Path) -> Result<std::fs::File> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create session log dir {}", parent.display())
            })?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("failed to open session log {}", log_path.display()))
    }

    fn session_jobs() -> &'static Mutex<HashMap<u32, Job>> {
        static SESSION_JOBS: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
        SESSION_JOBS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    struct Job(HANDLE);

    unsafe impl Send for Job {}

    impl Job {
        fn create() -> Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle == std::ptr::null_mut() {
                return Err(anyhow!("failed to create Windows Job Object"));
            }
            Ok(Self(handle))
        }

        fn apply_limits(&self, pids_max: u32) -> Result<()> {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if pids_max > 0 {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = pids_max;
            }
            let ok = unsafe {
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(anyhow!("failed to configure Windows Job Object limits"));
            }
            Ok(())
        }

        fn assign(&self, process: HANDLE) -> Result<()> {
            let ok = unsafe { AssignProcessToJobObject(self.0, process) };
            if ok == 0 {
                return Err(anyhow!("failed to assign process to Windows Job Object"));
            }
            Ok(())
        }

        fn terminate(&self, exit_code: u32) -> Result<()> {
            let ok = unsafe { TerminateJobObject(self.0, exit_code) };
            if ok == 0 {
                return Err(anyhow!("failed to terminate Windows Job Object"));
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn shell_join(command: &[String]) -> String {
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
                shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unix)]
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{shell_join, shell_quote, CommandRunner};

    #[test]
    fn shell_join_quotes_spaces_and_quotes() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "echo 'hello world'".into()]),
            "bash -lc 'echo '\\''hello world'\\'''"
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("shell's dev"), "'shell'\\''s dev'");
    }

    #[test]
    fn shell_join_preserves_empty_arguments() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "".into()]),
            "bash -lc ''"
        );
    }

    #[tokio::test]
    async fn append_to_file_creates_parent_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/session.log");

        CommandRunner::append_to_file(&path, "first\n")
            .await
            .unwrap();
        CommandRunner::append_to_file(&path, "second\n")
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "first\nsecond\n"
        );
    }
}
