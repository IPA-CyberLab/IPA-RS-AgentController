use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::io::Read;
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
    validate_view_runtime(&view_root, &cwd, program, &network)?;
    enter_view(&view_root, &cwd, &network)?;
    let status = command_for_network(program, args, &network)
        .status()
        .with_context(|| format!("failed to execute {program} inside {}", view_root.display()))?;
    Ok(status.code().unwrap_or(128))
}

#[cfg(target_os = "macos")]
fn spawn_session(args: SessionArgs) -> Result<u32> {
    let (program, command_args) = split_command(&args.command)?;
    ensure_overlay_mounted(&args.view_root, &args.lower, &args.upper, &args.whiteouts)?;
    validate_view_runtime(&args.view_root, &args.cwd, program, &args.network)?;
    validate_enter_args(&args.view_root, &args.cwd, &args.network)?;
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
    let mut command = command_for_network(program, command_args, &args.network);
    command
        .env("AGENT_NETWORK", &args.network)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(move || {
            enter_view_for_child(&args.view_root, &args.cwd).map_err(std::io::Error::other)
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

#[cfg(any(target_os = "macos", test))]
fn validate_view_runtime(view_root: &Path, cwd: &Path, program: &str, network: &str) -> Result<()> {
    let mut required = vec![
        Path::new("/bin"),
        Path::new("/usr"),
        Path::new("/usr/bin/env"),
        Path::new("/System"),
        Path::new("/Library"),
        Path::new("/private"),
        Path::new("/dev"),
        cwd,
    ];
    if program.starts_with('/') {
        required.push(Path::new(program));
    }
    if network == "none" {
        required.push(Path::new("/usr/bin/sandbox-exec"));
    }
    for path in required {
        let host_path = path_in_view_root(view_root, path)?;
        if !host_path.exists() {
            bail!(
                "path-preserving view at {} is missing required chroot path {}; macOS system fallback roots are not mounted correctly",
                view_root.display(),
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", test)))]
fn validate_view_runtime(
    _view_root: &Path,
    _cwd: &Path,
    _program: &str,
    _network: &str,
) -> Result<()> {
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn path_in_view_root(view_root: &Path, absolute_path: &Path) -> Result<PathBuf> {
    if !absolute_path.is_absolute() {
        bail!("chroot path must be absolute: {}", absolute_path.display());
    }
    Ok(absolute_path
        .strip_prefix("/")
        .map(|relative| view_root.join(relative))
        .unwrap_or_else(|_| view_root.to_path_buf()))
}

fn enter_view(view_root: &Path, cwd: &Path, network: &str) -> Result<()> {
    validate_enter_args(view_root, cwd, network)?;
    enter_view_for_child(view_root, cwd)?;
    std::env::set_var("AGENT_NETWORK", network);
    Ok(())
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

    if overlay_is_ready(view_root) {
        return Ok(());
    }

    let mut child = Command::new(overlayfs_program())
        .arg("mount")
        .arg("--mount-point")
        .arg(view_root)
        .arg("--lower")
        .arg(lower)
        .arg("--upper")
        .arg(upper)
        .arg("--whiteouts")
        .arg(whiteouts)
        .args(
            system_fallback_roots()
                .iter()
                .flat_map(|path| ["--fallback-root".into(), path.display().to_string()]),
        )
        .arg("--fs-name")
        .arg("agent-overlayfs")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn agent-overlayfs mount helper")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if overlay_is_ready(view_root) {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| "failed to poll agent-overlayfs mount helper")?
        {
            let stderr = child_stderr(&mut child)?;
            bail!(
                "agent-overlayfs exited before {} became ready: {}{}",
                view_root.display(),
                status_display(status.code()),
                stderr_suffix(&stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let stderr = child_stderr(&mut child).unwrap_or_default();
    bail!(
        "agent-overlayfs did not become ready at {} within 10s{}",
        view_root.display(),
        stderr_suffix(&stderr)
    )
}

#[cfg(target_os = "macos")]
fn child_stderr(child: &mut std::process::Child) -> Result<String> {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)
            .with_context(|| "failed to read agent-overlayfs stderr")?;
    }
    Ok(stderr)
}

#[cfg(any(target_os = "macos", test))]
fn status_display(code: Option<i32>) -> String {
    code.map(|code| format!("status {code}"))
        .unwrap_or_else(|| "terminated by signal".to_string())
}

#[cfg(any(target_os = "macos", test))]
fn stderr_suffix(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

#[cfg(target_os = "macos")]
fn overlay_is_ready(view_root: &Path) -> bool {
    view_root.join(".agent-overlayfs-ready").is_file() && overlay_mount_is_active(view_root)
}

#[cfg(target_os = "macos")]
fn overlay_mount_is_active(view_root: &Path) -> bool {
    let Ok(output) = Command::new("/sbin/mount").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let mount_point = view_root.display().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| overlay_mount_line_matches(line, &mount_point))
}

#[cfg(any(target_os = "macos", test))]
fn overlay_mount_line_matches(line: &str, mount_point: &str) -> bool {
    line.contains("agent-overlayfs")
        && (line.contains(&format!(" on {mount_point} ("))
            || line.contains(&format!(" on {} (", escape_mount_path(mount_point))))
}

#[cfg(any(target_os = "macos", test))]
fn escape_mount_path(path: &str) -> String {
    path.replace(' ', "\\040")
}

#[cfg(target_os = "macos")]
fn overlayfs_program() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("agent-overlayfs");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("agent-overlayfs")
}

#[cfg(target_os = "macos")]
fn system_fallback_roots() -> Vec<PathBuf> {
    [
        "/bin", "/sbin", "/usr", "/System", "/Library", "/private", "/dev", "/etc", "/var", "/tmp",
        "/opt",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect()
}

fn command_for_network(program: &str, args: &[String], network: &str) -> Command {
    if network == "none" {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-p")
            .arg("(version 1)\n(allow default)\n(deny network*)")
            .arg(program)
            .args(args);
        command
    } else {
        let mut command = Command::new(program);
        command.args(args);
        command
    }
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
fn enter_view_for_child(view_root: &Path, cwd: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let view_root_c = CString::new(view_root.as_os_str().as_bytes())?;
    let cwd_c = CString::new(cwd.as_os_str().as_bytes())?;
    let uid = target_uid();
    let gid = target_gid();
    unsafe {
        if geteuid() != 0 {
            bail!("agent-viewd must run as root for chroot setup; install the macOS privileged helper");
        }
        if chroot(view_root_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to chroot {}", view_root.display()));
        }
        if chdir(cwd_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to chdir preserved cwd {}", cwd.display()));
        }
        if let Some(gid) = gid {
            if setgroups(0, std::ptr::null()) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| "failed to drop supplementary groups");
            }
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
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn enter_view_for_child(_view_root: &Path, _cwd: &Path) -> Result<()> {
    bail!("agent-viewd path-preserving views are supported only on macOS")
}

#[cfg(target_os = "macos")]
fn target_uid() -> Option<u32> {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            let uid = unsafe { getuid() };
            (uid != 0).then_some(uid)
        })
}

#[cfg(target_os = "macos")]
fn target_gid() -> Option<u32> {
    std::env::var("SUDO_GID")
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            let gid = unsafe { getgid() };
            (gid != 0).then_some(gid)
        })
}

#[cfg(target_os = "macos")]
extern "C" {
    fn chroot(path: *const std::os::raw::c_char) -> std::os::raw::c_int;
    fn chdir(path: *const std::os::raw::c_char) -> std::os::raw::c_int;
    fn setuid(uid: u32) -> std::os::raw::c_int;
    fn setgid(gid: u32) -> std::os::raw::c_int;
    fn setgroups(ngroups: std::os::raw::c_int, groups: *const u32) -> std::os::raw::c_int;
    fn geteuid() -> u32;
    fn getuid() -> u32;
    fn getgid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;
    use std::fs;
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

    #[test]
    fn network_none_wraps_command_with_sandbox_exec() {
        let command = super::command_for_network("/bin/echo", &["ok".to_string()], "none");

        assert_eq!(command.get_program(), "/usr/bin/sandbox-exec");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"(version 1)\n(allow default)\n(deny network*)".to_string()));
        assert!(args.contains(&"/bin/echo".to_string()));
        assert!(args.contains(&"ok".to_string()));
    }

    #[test]
    fn host_network_runs_command_directly() {
        let command = super::command_for_network("/bin/echo", &["ok".to_string()], "host");

        assert_eq!(command.get_program(), "/bin/echo");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["ok".to_string()]
        );
    }

    #[test]
    fn overlay_mount_line_matches_agent_overlayfs_mount() {
        assert!(super::overlay_mount_line_matches(
            "agent-overlayfs on /Users/me/.agentfs/envs/codex/view-root (macfuse, local)",
            "/Users/me/.agentfs/envs/codex/view-root"
        ));
        assert!(super::overlay_mount_line_matches(
            "agent-overlayfs on /Users/me/My\\040Project/view-root (macfuse, local)",
            "/Users/me/My Project/view-root"
        ));
        assert!(!super::overlay_mount_line_matches(
            "/dev/disk3s1 on /Users/me/.agentfs/envs/codex/view-root (apfs, local)",
            "/Users/me/.agentfs/envs/codex/view-root"
        ));
    }

    #[test]
    fn mount_helper_error_message_parts_are_compact() {
        assert_eq!(super::status_display(Some(42)), "status 42");
        assert_eq!(super::status_display(None), "terminated by signal");
        assert_eq!(super::stderr_suffix(""), "");
        assert_eq!(super::stderr_suffix("  mount failed\n"), ": mount failed");
    }

    #[test]
    fn view_runtime_requires_macos_system_paths_and_requested_program() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        fs::write(view_root.join("bin/zsh"), "").unwrap();

        super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "host",
        )
        .unwrap();

        fs::remove_file(view_root.join("usr/bin/env")).unwrap();
        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "host",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/usr/bin/env"));
    }

    #[test]
    fn view_runtime_requires_sandbox_exec_for_network_none() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        fs::write(view_root.join("bin/zsh"), "").unwrap();

        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "none",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/usr/bin/sandbox-exec"));

        fs::write(view_root.join("usr/bin/sandbox-exec"), "").unwrap();
        super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "none",
        )
        .unwrap();
    }

    fn seed_required_view_paths(view_root: &std::path::Path) {
        for path in ["bin", "usr/bin", "System", "Library", "private", "dev"] {
            fs::create_dir_all(view_root.join(path)).unwrap();
        }
        fs::write(view_root.join("usr/bin/env"), "").unwrap();
    }
}
