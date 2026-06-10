use crate::model::Limits;
use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::path::Path;
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
            std::fs::File::open(parent)
                .with_context(|| format!("failed to open log dir {}", parent.display()))?
                .sync_all()
                .with_context(|| format!("failed to sync log dir {}", parent.display()))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
async fn run_desktop_isolated(
    cwd: &Path,
    program: &str,
    args: &[String],
    _limits: &Limits,
) -> Result<CmdOutput> {
    let profile = macos_sandbox_profile(cwd);
    let mut command = Command::new("sandbox-exec");
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
fn macos_sandbox_profile(rootfs: &Path) -> String {
    let rootfs = scheme_string(rootfs.display().to_string().as_str());
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath "{rootfs}"))
(deny network*)
"#
    )
}

#[cfg(target_os = "macos")]
fn scheme_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

#[cfg(not(any(target_os = "macos", windows)))]
async fn run_desktop_isolated(
    cwd: &Path,
    program: &str,
    args: &[String],
    _limits: &Limits,
) -> Result<CmdOutput> {
    CommandRunner.run_in_dir(cwd, program, args).await
}

#[cfg(windows)]
mod windows_job {
    use super::CmdOutput;
    use crate::model::Limits;
    use anyhow::{anyhow, Context, Result};
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use std::process::{Command as StdCommand, Stdio};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
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

    struct Job(HANDLE);

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
