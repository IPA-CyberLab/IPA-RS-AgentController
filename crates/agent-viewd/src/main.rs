use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "agent-viewd",
    about = "Privileged macOS view helper for path-preserving agent overlays"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    Exec(EnterArgs),
    Shell(ShellArgs),
    Session(SessionArgs),
}

#[derive(Debug, Parser)]
struct EnterArgs {
    #[arg(long)]
    view_root: PathBuf,
    #[arg(long)]
    lower: PathBuf,
    #[arg(long)]
    upper: PathBuf,
    #[arg(long)]
    whiteouts: PathBuf,
    #[arg(long)]
    cwd: PathBuf,
    #[arg(long, default_value = "host")]
    network: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Parser)]
struct ShellArgs {
    #[arg(long)]
    view_root: PathBuf,
    #[arg(long)]
    lower: PathBuf,
    #[arg(long)]
    upper: PathBuf,
    #[arg(long)]
    whiteouts: PathBuf,
    #[arg(long)]
    cwd: PathBuf,
    #[arg(long)]
    env_id: String,
    #[arg(long, default_value = "host")]
    network: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Parser)]
struct SessionArgs {
    #[arg(long)]
    view_root: PathBuf,
    #[arg(long)]
    lower: PathBuf,
    #[arg(long)]
    upper: PathBuf,
    #[arg(long)]
    whiteouts: PathBuf,
    #[arg(long)]
    cwd: PathBuf,
    #[arg(long, default_value = "host")]
    network: String,
    #[arg(long)]
    log_path: PathBuf,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        CommandKind::Exec(args) => {
            let status = enter_and_run(
                args.view_root,
                args.lower,
                args.upper,
                args.whiteouts,
                args.cwd,
                args.network,
                args.command,
            )?;
            std::process::exit(status);
        }
        CommandKind::Shell(args) => {
            let mut command = args.command;
            apply_prompt_env(&mut command, &args.env_id);
            let status = enter_and_run(
                args.view_root,
                args.lower,
                args.upper,
                args.whiteouts,
                args.cwd,
                args.network,
                command,
            )?;
            std::process::exit(status);
        }
        CommandKind::Session(args) => {
            let pid = spawn_session(args)?;
            println!("{pid}");
            Ok(())
        }
    }
}

fn apply_prompt_env(command: &mut [String], env_id: &str) {
    let Some(shell) = command.first() else {
        return;
    };
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    if shell_name.contains("zsh") {
        std::env::set_var("PROMPT", format!("%F{{green}}{env_id}%f@%m %1~ %# "));
    } else {
        std::env::set_var(
            "PS1",
            format!("\\[\\033[32m\\]{env_id}\\[\\033[0m\\]@\\h \\w \\\\$ "),
        );
    }
}

fn enter_and_run(
    view_root: PathBuf,
    lower: PathBuf,
    upper: PathBuf,
    whiteouts: PathBuf,
    cwd: PathBuf,
    network: String,
    command: Vec<String>,
) -> Result<i32> {
    let (program, args) = split_command(&command)?;
    ensure_overlay_mounted(&view_root, &lower, &upper, &whiteouts)?;
    enter_view(&view_root, &cwd, &network)?;
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to execute {program} inside {}", view_root.display()))?;
    Ok(status.code().unwrap_or(128))
}

#[cfg(target_os = "macos")]
fn spawn_session(args: SessionArgs) -> Result<u32> {
    let (program, command_args) = split_command(&args.command)?;
    validate_enter_args(&args.view_root, &args.cwd, &args.network)?;
    ensure_overlay_mounted(&args.view_root, &args.lower, &args.upper, &args.whiteouts)?;
    if let Some(parent) = args.log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log dir {}", parent.display()))?;
    }
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_path)
        .with_context(|| format!("failed to open log {}", args.log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to clone log {}", args.log_path.display()))?;
    let mut command = Command::new(program);
    command
        .args(command_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(move || {
            enter_view_for_child(&args.view_root, &args.cwd, &args.network)
                .map_err(std::io::Error::other)
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {program} inside path-preserving view"))?;
    Ok(child.id())
}

#[cfg(not(target_os = "macos"))]
fn spawn_session(_args: SessionArgs) -> Result<u32> {
    bail!("agent-viewd path-preserving sessions are supported only on macOS")
}

fn split_command(command: &[String]) -> Result<(&str, &[String])> {
    command
        .split_first()
        .map(|(program, args)| (program.as_str(), args))
        .ok_or_else(|| anyhow!("command cannot be empty"))
}

fn enter_view(view_root: &Path, cwd: &Path, network: &str) -> Result<()> {
    validate_enter_args(view_root, cwd, network)?;
    enter_view_for_child(view_root, cwd, network)
}

fn validate_enter_args(view_root: &Path, cwd: &Path, network: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("agent-viewd path-preserving views are supported only on macOS");
    }
    if !view_root.is_absolute() {
        bail!("view-root must be absolute: {}", view_root.display());
    }
    if !cwd.is_absolute() {
        bail!("preserved cwd must be absolute: {}", cwd.display());
    }
    if !matches!(network, "host" | "bridge" | "none") {
        bail!("unsupported network mode {network}; use host, bridge, or none");
    }
    if !view_root.is_dir() {
        bail!("view-root {} does not exist", view_root.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_overlay_mounted(
    view_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<()> {
    for (name, path) in [
        ("view-root", view_root),
        ("lower", lower),
        ("upper", upper),
        ("whiteouts", whiteouts),
    ] {
        if !path.is_absolute() {
            bail!("{name} must be absolute: {}", path.display());
        }
    }
    std::fs::create_dir_all(view_root)
        .with_context(|| format!("failed to create view-root {}", view_root.display()))?;
    std::fs::create_dir_all(lower)
        .with_context(|| format!("failed to create lower {}", lower.display()))?;
    std::fs::create_dir_all(upper)
        .with_context(|| format!("failed to create upper {}", upper.display()))?;
    std::fs::create_dir_all(whiteouts)
        .with_context(|| format!("failed to create whiteouts {}", whiteouts.display()))?;

    let ready = view_root.join(".agent-overlayfs-ready");
    if ready.is_file() {
        return Ok(());
    }

    Command::new("agent-overlayfs")
        .arg("mount")
        .arg("--mount-point")
        .arg(view_root)
        .arg("--lower")
        .arg(lower)
        .arg("--upper")
        .arg(upper)
        .arg("--whiteouts")
        .arg(whiteouts)
        .arg("--fs-name")
        .arg("agent-overlayfs")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| "failed to spawn agent-overlayfs mount helper")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ready.is_file() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "agent-overlayfs did not become ready at {} within 5s",
        view_root.display()
    )
}

#[cfg(not(target_os = "macos"))]
fn ensure_overlay_mounted(
    _view_root: &Path,
    _lower: &Path,
    _upper: &Path,
    _whiteouts: &Path,
) -> Result<()> {
    bail!("agent-overlayfs is supported only on macOS")
}

#[cfg(target_os = "macos")]
fn enter_view_for_child(view_root: &Path, cwd: &Path, network: &str) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let view_root_c = CString::new(view_root.as_os_str().as_bytes())?;
    let cwd_c = CString::new(cwd.as_os_str().as_bytes())?;
    let uid = target_uid();
    let gid = target_gid();
    unsafe {
        if chroot(view_root_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to chroot {}", view_root.display()));
        }
        if chdir(cwd_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to chdir preserved cwd {}", cwd.display()));
        }
        if let Some(gid) = gid {
            if setgid(gid) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("failed to drop gid to {gid}"));
            }
        }
        if let Some(uid) = uid {
            if setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("failed to drop uid to {uid}"));
            }
        }
    }
    std::env::set_var("AGENT_NETWORK", network);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn enter_view_for_child(_view_root: &Path, _cwd: &Path, _network: &str) -> Result<()> {
    bail!("agent-viewd path-preserving views are supported only on macOS")
}

#[cfg(target_os = "macos")]
fn target_uid() -> Option<u32> {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse().ok())
}

#[cfg(target_os = "macos")]
fn target_gid() -> Option<u32> {
    std::env::var("SUDO_GID")
        .ok()
        .and_then(|value| value.parse().ok())
}

#[cfg(target_os = "macos")]
extern "C" {
    fn chroot(path: *const std::os::raw::c_char) -> std::os::raw::c_int;
    fn chdir(path: *const std::os::raw::c_char) -> std::os::raw::c_int;
    fn setuid(uid: u32) -> std::os::raw::c_int;
    fn setgid(gid: u32) -> std::os::raw::c_int;
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn parses_exec_with_view_root_and_preserved_cwd() {
        let cli = Cli::parse_from([
            "agent-viewd",
            "exec",
            "--view-root",
            "/Users/mizuame/.agentfs/envs/codex/view-root",
            "--lower",
            "/Users/mizuame/.agentfs/envs/codex/lower",
            "--upper",
            "/Users/mizuame/.agentfs/envs/codex/upper",
            "--whiteouts",
            "/Users/mizuame/.agentfs/envs/codex/whiteouts",
            "--cwd",
            "/Users/mizuame/Desktop/project",
            "--network",
            "none",
            "--",
            "/bin/pwd",
        ]);

        let super::CommandKind::Exec(args) = cli.command else {
            panic!("expected exec command");
        };
        assert_eq!(
            args.view_root,
            PathBuf::from("/Users/mizuame/.agentfs/envs/codex/view-root")
        );
        assert_eq!(
            args.lower,
            PathBuf::from("/Users/mizuame/.agentfs/envs/codex/lower")
        );
        assert_eq!(
            args.upper,
            PathBuf::from("/Users/mizuame/.agentfs/envs/codex/upper")
        );
        assert_eq!(
            args.whiteouts,
            PathBuf::from("/Users/mizuame/.agentfs/envs/codex/whiteouts")
        );
        assert_eq!(args.cwd, PathBuf::from("/Users/mizuame/Desktop/project"));
        assert_eq!(args.network, "none");
        assert_eq!(args.command, vec!["/bin/pwd"]);
    }
}
