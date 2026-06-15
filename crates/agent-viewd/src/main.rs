use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(any(target_os = "macos", test))]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
#[cfg(any(target_os = "macos", test))]
use std::path::Component;
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
    source_root: Option<PathBuf>,
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
    source_root: Option<PathBuf>,
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
    source_root: Option<PathBuf>,
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
                args.source_root,
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
                args.source_root,
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
        std::env::set_var("PROMPT", format!("%F{{green}}{env_id}%f@%m %~ %# "));
    } else {
        std::env::set_var(
            "PS1",
            format!("\\[\\033[32m\\]{env_id}\\[\\033[0m\\]@\\h \\w \\\\$ "),
        );
    }
}

fn enter_and_run(
    view_root: PathBuf,
    source_root: Option<PathBuf>,
    lower: PathBuf,
    upper: PathBuf,
    whiteouts: PathBuf,
    cwd: PathBuf,
    network: String,
    command: Vec<String>,
) -> Result<i32> {
    let (program, args) = split_command(&command)?;
    validate_network_mode(&network)?;
    if let Some(source_root) = source_root {
        let visible_root = source_visible_root(&source_root);
        let mount = ensure_source_overlay_mounted(
            &view_root,
            &visible_root,
            &source_root,
            &lower,
            &upper,
            &whiteouts,
        )?;
        validate_direct_runtime(&source_root, &cwd, program)?;
        let mounted_cwd = source_view_path(&view_root, &visible_root, &cwd)?;
        prepare_source_view_workspace(&view_root, &mounted_cwd)?;
        let mut command = command_for_direct_mount(program, args, &network, &view_root);
        prepare_direct_child(&mut command, &mounted_cwd);
        let status = command
            .env("PWD", &cwd)
            .env("AGENT_SOURCE_ROOT", &source_root)
            .env("AGENT_VISIBLE_ROOT", &visible_root)
            .env("AGENT_VIEW_ROOT", &view_root)
            .status()
            .with_context(|| {
                format!(
                    "failed to execute {program} inside path-preserving source {}",
                    source_root.display()
                )
            })?;
        drop(mount);
        return Ok(status.code().unwrap_or(128));
    }
    ensure_overlay_mounted(&view_root, &lower, &upper, &whiteouts)?;
    validate_view_runtime(&view_root, &cwd, program, &network)?;
    enter_view(&view_root, &cwd, &network)?;
    let status = command_for_chroot_network(program, args, &network)
        .status()
        .with_context(|| format!("failed to execute {program} inside {}", view_root.display()))?;
    Ok(status.code().unwrap_or(128))
}

#[cfg(target_os = "macos")]
fn spawn_session(args: SessionArgs) -> Result<u32> {
    let (program, command_args) = split_command(&args.command)?;
    validate_network_mode(&args.network)?;
    if let Some(source_root) = args.source_root.as_ref() {
        let visible_root = source_visible_root(source_root);
        let _mount = ensure_source_overlay_mounted(
            &args.view_root,
            &visible_root,
            source_root,
            &args.lower,
            &args.upper,
            &args.whiteouts,
        )?;
        validate_direct_runtime(source_root, &args.cwd, program)?;
        validate_session_log_path(&args.view_root, &args.log_path)?;
        let mounted_cwd = source_view_path(&args.view_root, &visible_root, &args.cwd)?;
        prepare_source_view_workspace(&args.view_root, &mounted_cwd)?;
        let stdout = open_session_log(&args.log_path)?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("failed to clone log {}", args.log_path.display()))?;
        let mut command =
            command_for_direct_mount(program, command_args, &args.network, &args.view_root);
        prepare_direct_child(&mut command, &mounted_cwd);
        command
            .env("PWD", &args.cwd)
            .env("AGENT_SOURCE_ROOT", source_root)
            .env("AGENT_VISIBLE_ROOT", &visible_root)
            .env("AGENT_VIEW_ROOT", &args.view_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn {program} inside path-preserving source"))?;
        std::mem::forget(_mount);
        return Ok(child.id());
    }
    ensure_overlay_mounted(&args.view_root, &args.lower, &args.upper, &args.whiteouts)?;
    validate_view_runtime(&args.view_root, &args.cwd, program, &args.network)?;
    validate_enter_args(&args.view_root, &args.cwd, &args.network)?;
    validate_session_log_path(&args.view_root, &args.log_path)?;
    let stdout = open_session_log(&args.log_path)?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to clone log {}", args.log_path.display()))?;
    let mut command = command_for_chroot_network(program, command_args, &args.network);
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
    for path in [
        Path::new("/bin"),
        Path::new("/usr"),
        Path::new("/System"),
        Path::new("/Library"),
        Path::new("/private"),
        Path::new("/dev"),
        cwd,
    ] {
        validate_view_dir(view_root, path)?;
    }
    validate_view_path_exists(view_root, Path::new("/dev/null"))?;
    validate_view_path_exists(view_root, Path::new("/usr/lib/dyld"))?;
    validate_view_executable(view_root, Path::new("/usr/bin/env"))?;
    if program.starts_with('/') {
        validate_view_executable(view_root, Path::new(program))?;
    }
    if network == "none" {
        validate_view_executable(view_root, Path::new("/usr/bin/sandbox-exec"))?;
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

#[cfg(any(target_os = "macos", test))]
fn validate_view_dir(view_root: &Path, path: &Path) -> Result<()> {
    let host_path = path_in_view_root(view_root, path)?;
    if !host_path.is_dir() {
        bail!(
            "path-preserving view at {} is missing required chroot directory {}; macOS system fallback roots are not mounted correctly",
            view_root.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_view_path_exists(view_root: &Path, path: &Path) -> Result<()> {
    let host_path = path_in_view_root(view_root, path)?;
    if !host_path.exists() {
        bail!(
            "path-preserving view at {} is missing required chroot path {}; macOS system fallback roots are not mounted correctly",
            view_root.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_view_executable(view_root: &Path, path: &Path) -> Result<()> {
    let host_path = path_in_view_root(view_root, path)?;
    let metadata = std::fs::metadata(&host_path).with_context(|| {
        format!(
            "path-preserving view at {} is missing required executable {}; macOS system fallback roots are not mounted correctly",
            view_root.display(),
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "path-preserving view at {} has non-executable required chroot path {}; macOS system fallback roots are not mounted correctly",
            view_root.display(),
            path.display()
        );
    }
    Ok(())
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
    validate_network_mode(network)?;
    if !view_root.is_dir() {
        bail!("view-root {} does not exist", view_root.display());
    }
    Ok(())
}

fn validate_network_mode(network: &str) -> Result<()> {
    if matches!(network, "host" | "none") {
        return Ok(());
    }
    if network == "bridge" {
        bail!("network mode bridge is not supported by macOS path-preserving views yet; use host or none");
    }
    bail!("unsupported network mode {network}; use host or none")
}

#[cfg(target_os = "macos")]
fn ensure_overlay_mounted(
    view_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<()> {
    validate_overlay_paths(view_root, lower, upper, whiteouts)?;
    let _mount = ensure_overlay_mounted_at(view_root, None, lower, upper, whiteouts)?;
    std::mem::forget(_mount);
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_source_overlay_mounted(
    view_root: &Path,
    visible_root: &Path,
    source_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<MountedOverlay> {
    if overlay_mount_is_active(source_root) {
        bail!(
            "source-root {} is still mounted directly by an active legacy macOS path-preserving env; exit that shell before starting another env for the same source",
            source_root.display()
        );
    }
    validate_overlay_paths(view_root, lower, upper, whiteouts)?;
    validate_source_overlay_paths(source_root, lower, upper, whiteouts)?;
    ensure_overlay_mounted_at(view_root, Some(visible_root), lower, upper, whiteouts)
}

#[cfg(target_os = "macos")]
fn ensure_overlay_mounted_at(
    mount_point: &Path,
    visible_root: Option<&Path>,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<MountedOverlay> {
    for (name, path) in [
        ("mount-point", mount_point),
        ("lower", lower),
        ("upper", upper),
        ("whiteouts", whiteouts),
    ] {
        if !path.is_absolute() {
            bail!("{name} must be absolute: {}", path.display());
        }
    }
    if let Some(visible_root) = visible_root {
        if !visible_root.is_absolute() {
            bail!("visible-root must be absolute: {}", visible_root.display());
        }
    }
    std::fs::create_dir_all(mount_point)
        .with_context(|| format!("failed to create mount-point {}", mount_point.display()))?;
    std::fs::create_dir_all(lower)
        .with_context(|| format!("failed to create lower {}", lower.display()))?;
    std::fs::create_dir_all(upper)
        .with_context(|| format!("failed to create upper {}", upper.display()))?;
    std::fs::create_dir_all(whiteouts)
        .with_context(|| format!("failed to create whiteouts {}", whiteouts.display()))?;

    if overlay_is_ready(mount_point) {
        return Ok(MountedOverlay::borrowed(mount_point));
    }

    let stderr_path = overlay_stderr_path(mount_point, visible_root, lower)?;
    if let Some(parent) = stderr_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create agent-overlayfs stderr log dir {}",
                parent.display()
            )
        })?;
    }
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&stderr_path)
        .with_context(|| {
            format!(
                "failed to open agent-overlayfs stderr log {}",
                stderr_path.display()
            )
        })?;
    let mut child_command = Command::new(overlayfs_program());
    child_command
        .arg("mount")
        .arg("--mount-point")
        .arg(mount_point);
    if let Some(visible_root) = visible_root {
        child_command.arg("--visible-root").arg(visible_root);
    }
    child_command
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
        .stderr(Stdio::from(stderr_file));
    unsafe {
        child_command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            drop_to_target_user().map_err(std::io::Error::other)
        });
    }
    let mut child = child_command
        .spawn()
        .with_context(|| "failed to spawn agent-overlayfs mount helper")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if overlay_is_ready(mount_point) {
            return Ok(MountedOverlay::owned(mount_point));
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| "failed to poll agent-overlayfs mount helper")?
        {
            let stderr = read_and_remove_file(&stderr_path);
            bail!(
                "agent-overlayfs exited before {} became ready: {}{}",
                mount_point.display(),
                status_display(status.code()),
                stderr_suffix(&stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let stderr = read_and_remove_file(&stderr_path);
    let diagnostics = overlay_readiness_diagnostics(mount_point, child.id(), &stderr);
    kill_process_group(child.id());
    let _ = child.kill();
    bail!(
        "agent-overlayfs did not become ready at {} within 10s; mount helper was terminated{}",
        mount_point.display(),
        diagnostics
    )
}

#[cfg(target_os = "macos")]
struct MountedOverlay {
    mount_point: PathBuf,
    owned: bool,
}

#[cfg(target_os = "macos")]
impl MountedOverlay {
    fn owned(mount_point: &Path) -> Self {
        Self {
            mount_point: mount_point.to_path_buf(),
            owned: true,
        }
    }

    fn borrowed(mount_point: &Path) -> Self {
        Self {
            mount_point: mount_point.to_path_buf(),
            owned: false,
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedOverlay {
    fn drop(&mut self) {
        if self.owned {
            unmount_path(&self.mount_point);
        }
    }
}

#[cfg(target_os = "macos")]
fn unmount_path(mount_point: &Path) {
    if !overlay_mount_is_active(mount_point) {
        return;
    }
    let _ = Command::new("/sbin/umount").arg(mount_point).status();
    if overlay_mount_is_active(mount_point) {
        let _ = Command::new("diskutil")
            .arg("unmount")
            .arg("force")
            .arg(mount_point)
            .status();
    }
}

#[cfg(target_os = "macos")]
fn kill_process_group(pid: u32) {
    if pid > i32::MAX as u32 {
        return;
    }
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(target_os = "macos")]
fn overlay_stderr_path(
    mount_point: &Path,
    visible_root: Option<&Path>,
    lower: &Path,
) -> Result<PathBuf> {
    let env_dir = if visible_root.is_some() {
        lower
            .parent()
            .ok_or_else(|| anyhow!("lower must be inside an env directory"))?
    } else {
        mount_point
            .parent()
            .ok_or_else(|| anyhow!("view-root must be inside an env directory"))?
    };
    Ok(env_dir.join("logs").join("agent-overlayfs-mount.stderr"))
}

#[cfg(target_os = "macos")]
fn overlay_readiness_diagnostics(view_root: &Path, child_pid: u32, stderr: &str) -> String {
    let mount_lines = command_stdout("/sbin/mount", &[])
        .map(|stdout| {
            let matches = stdout
                .lines()
                .filter(|line| line.contains(&view_root.display().to_string()))
                .collect::<Vec<_>>();
            if matches.is_empty() {
                "mount_lines=<none>".to_string()
            } else {
                format!("mount_lines={}", matches.join(" | "))
            }
        })
        .unwrap_or_else(|error| format!("mount_error={error}"));
    let ps_output = command_stdout(
        "/bin/ps",
        &[
            "-o",
            "pid,ppid,pgid,stat,command",
            "-p",
            &child_pid.to_string(),
        ],
    )
    .map(|stdout| format!("helper_ps={}", stdout.trim()))
    .unwrap_or_else(|error| format!("helper_ps_error={error}"));
    format!(
        "\nreadiness diagnostics: {mount_lines}; {}; {ps_output}",
        stderr_diagnostic(stderr)
    )
}

#[cfg(target_os = "macos")]
fn stderr_diagnostic(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "helper_stderr=<empty>".to_string()
    } else {
        format!("helper_stderr={trimmed}")
    }
}

#[cfg(target_os = "macos")]
fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}{}",
            status_display(output.status.code()),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(target_os = "macos")]
fn read_and_remove_file(path: &Path) -> String {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    content
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
    overlay_mount_is_active(view_root)
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
    system_fallback_root_candidates()
        .iter()
        .copied()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn system_fallback_root_candidates() -> &'static [&'static str] {
    &[
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/lib",
        "/usr/share",
        "/System",
        "/Library/Filesystems",
        "/dev/null",
        "/dev/random",
        "/dev/tty",
        "/dev/urandom",
        "/dev/zero",
        "/etc/hosts",
        "/etc/protocols",
        "/etc/resolv.conf",
        "/etc/services",
        "/etc/shells",
        "/etc/zprofile",
        "/etc/zshenv",
        "/etc/zshrc",
        "/var/tmp",
        "/tmp",
        "/private/etc/hosts",
        "/private/etc/protocols",
        "/private/etc/resolv.conf",
        "/private/etc/services",
        "/private/etc/shells",
        "/private/etc/zprofile",
        "/private/etc/zshenv",
        "/private/etc/zshrc",
        "/private/tmp",
        "/private/var/tmp",
    ]
}

fn source_visible_root(source_root: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    source_visible_root_with_home(source_root, home.as_deref())
}

fn source_visible_root_with_home(source_root: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if home.is_absolute() && source_root.starts_with(home) {
            return home.to_path_buf();
        }
    }
    source_root.to_path_buf()
}

fn source_view_path(view_root: &Path, visible_root: &Path, path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("preserved cwd must be absolute: {}", path.display());
    }
    let relative = path.strip_prefix(visible_root).with_context(|| {
        format!(
            "preserved cwd {} must be inside visible-root {}",
            path.display(),
            visible_root.display()
        )
    })?;
    Ok(view_root.join(relative))
}

fn prepare_source_view_workspace(view_root: &Path, mounted_cwd: &Path) -> Result<()> {
    if !mounted_cwd.is_dir() {
        bail!(
            "path-preserving view at {} is missing cwd {}",
            view_root.display(),
            mounted_cwd.display()
        );
    }
    Ok(())
}

fn command_for_chroot_network(program: &str, args: &[String], network: &str) -> Command {
    let mut command = if network == "none" {
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
    };
    command
        .env("AGENT_NETWORK", network)
        .env("TMPDIR", "/tmp")
        .env("TMP", "/tmp")
        .env("TEMP", "/tmp");
    command
}

fn command_for_direct_mount(
    program: &str,
    args: &[String],
    network: &str,
    view_root: &Path,
) -> Command {
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-p")
        .arg(direct_mount_sandbox_profile(view_root, network))
        .arg(program)
        .args(args)
        .env("AGENT_NETWORK", network)
        .env("HOME", view_root)
        .env("ZDOTDIR", view_root)
        .env("TMPDIR", view_root)
        .env("TMP", view_root)
        .env("TEMP", view_root);
    if let Ok(host_home) = std::env::var("HOME") {
        command.env("HOST_HOME", host_home);
    }
    command
}

#[cfg(target_os = "macos")]
fn direct_mount_sandbox_profile(view_root: &Path, network: &str) -> String {
    let view_root = scheme_string(&view_root.display().to_string());
    let network_rule = if network == "none" {
        "(deny network*)"
    } else {
        "(allow network*)"
    };
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow file-read*)
(allow file-ioctl)
(allow file-write* (subpath "{view_root}"))
{network_rule}
"#
    )
}

#[cfg(not(target_os = "macos"))]
fn direct_mount_sandbox_profile(_view_root: &Path, _network: &str) -> String {
    String::new()
}

#[cfg(target_os = "macos")]
fn scheme_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(not(target_os = "macos"))]
fn scheme_string(value: &str) -> String {
    value.to_string()
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

#[cfg(not(target_os = "macos"))]
struct MountedOverlay;

#[cfg(not(target_os = "macos"))]
fn ensure_source_overlay_mounted(
    _view_root: &Path,
    _visible_root: &Path,
    _source_root: &Path,
    _lower: &Path,
    _upper: &Path,
    _whiteouts: &Path,
) -> Result<MountedOverlay> {
    bail!("agent-overlayfs is supported only on macOS")
}

#[cfg(not(target_os = "macos"))]
fn prepare_direct_child(_command: &mut Command, _cwd: &Path) {}

#[cfg(target_os = "macos")]
fn enter_view_for_child(view_root: &Path, cwd: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let view_root_c = CString::new(view_root.as_os_str().as_bytes())?;
    let cwd_c = CString::new(cwd.as_os_str().as_bytes())?;
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
    }
    drop_to_target_user()
}

#[cfg(target_os = "macos")]
fn prepare_direct_child(command: &mut Command, cwd: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let cwd = cwd.as_os_str().as_bytes().to_vec();
    unsafe {
        command.pre_exec(move || {
            drop_to_target_user().map_err(std::io::Error::other)?;
            let cwd = CString::new(cwd.as_slice()).map_err(std::io::Error::other)?;
            if chdir(cwd.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(target_os = "macos")]
fn drop_to_target_user() -> Result<()> {
    if unsafe { geteuid() } != 0 {
        return Ok(());
    }
    let uid = target_uid();
    let gid = target_gid();
    unsafe {
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

#[cfg(any(target_os = "macos", test))]
fn validate_overlay_paths(
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
        reject_dot_components(name, path)?;
        reject_existing_symlink_components(name, path)?;
    }

    let env_dir = view_root
        .parent()
        .ok_or_else(|| anyhow!("view-root must be inside an env directory"))?;
    let envs_dir = env_dir
        .parent()
        .ok_or_else(|| anyhow!("view-root env directory must be inside an envs directory"))?;
    if envs_dir.file_name().and_then(|name| name.to_str()) != Some("envs") {
        bail!(
            "path-preserving view-root must be under an agentfs envs directory: {}",
            view_root.display()
        );
    }

    for (name, path, expected_basename) in [
        ("view-root", view_root, "view-root"),
        ("lower", lower, "lower"),
        ("upper", upper, "upper"),
        ("whiteouts", whiteouts, "whiteouts"),
    ] {
        if path.parent() != Some(env_dir) {
            bail!(
                "{name} must be a sibling inside {}, got {}",
                env_dir.display(),
                path.display()
            );
        }
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_basename) {
            bail!(
                "{name} must be named {expected_basename}, got {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_source_overlay_paths(
    source_root: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<()> {
    if !source_root.is_absolute() {
        bail!("source-root must be absolute: {}", source_root.display());
    }
    reject_dot_components("source-root", source_root)?;
    reject_existing_symlink_components("source-root", source_root)?;
    if !source_root.is_dir() {
        bail!("source-root {} does not exist", source_root.display());
    }
    if source_root == Path::new("/") {
        bail!("source-root must not be /");
    }

    for (name, path) in [("lower", lower), ("upper", upper), ("whiteouts", whiteouts)] {
        if !path.is_absolute() {
            bail!("{name} must be absolute: {}", path.display());
        }
        reject_dot_components(name, path)?;
        reject_existing_symlink_components(name, path)?;
    }

    let env_dir = lower
        .parent()
        .ok_or_else(|| anyhow!("lower must be inside an env directory"))?;
    let envs_dir = env_dir
        .parent()
        .ok_or_else(|| anyhow!("lower env directory must be inside an envs directory"))?;
    if envs_dir.file_name().and_then(|name| name.to_str()) != Some("envs") {
        bail!(
            "path-preserving storage must be under an agentfs envs directory: {}",
            lower.display()
        );
    }
    for (name, path, expected_basename) in [
        ("lower", lower, "lower"),
        ("upper", upper, "upper"),
        ("whiteouts", whiteouts, "whiteouts"),
    ] {
        if path.parent() != Some(env_dir) {
            bail!(
                "{name} must be a sibling inside {}, got {}",
                env_dir.display(),
                path.display()
            );
        }
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_basename) {
            bail!(
                "{name} must be named {expected_basename}, got {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_direct_runtime(source_root: &Path, cwd: &Path, program: &str) -> Result<()> {
    if !cwd.starts_with(source_root) {
        bail!(
            "preserved cwd {} must be inside source-root {}",
            cwd.display(),
            source_root.display()
        );
    }
    if program.starts_with('/') {
        let metadata = std::fs::metadata(program)
            .with_context(|| format!("direct path-preserving command {program} is missing"))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            bail!("direct path-preserving command {program} is not executable");
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", test)))]
fn validate_direct_runtime(_source_root: &Path, _cwd: &Path, _program: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_session_log(log_path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log dir {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open log {}", log_path.display()))
}

#[cfg(any(target_os = "macos", test))]
fn validate_session_log_path(view_root: &Path, log_path: &Path) -> Result<()> {
    if !log_path.is_absolute() {
        bail!("log-path must be absolute: {}", log_path.display());
    }
    reject_dot_components("log-path", log_path)?;
    reject_existing_symlink_components("log-path", log_path)?;

    let env_dir = view_root
        .parent()
        .ok_or_else(|| anyhow!("view-root must be inside an env directory"))?;
    let expected_parent = env_dir.join("logs").join("sessions");
    if log_path.parent() != Some(expected_parent.as_path()) {
        bail!(
            "log-path must be inside {}, got {}",
            expected_parent.display(),
            log_path.display()
        );
    }
    if log_path.file_name().is_none() {
        bail!(
            "log-path must name a session log file: {}",
            log_path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn reject_dot_components(name: &str, path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => {
                bail!(
                    "{name} must not contain . or .. components: {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn reject_existing_symlink_components(name: &str, path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "{name} must not pass through symlink component {}",
                current.display()
            );
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn canonical_temp_root(temp: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(temp.path()).unwrap()
    }

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
        let command = super::command_for_chroot_network("/bin/echo", &["ok".to_string()], "none");

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
        let command = super::command_for_chroot_network("/bin/echo", &["ok".to_string()], "host");

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
    fn commands_use_chroot_local_runtime_env() {
        let command = super::command_for_chroot_network("/bin/echo", &["ok".to_string()], "host");
        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();

        assert!(envs.contains(&("AGENT_NETWORK".to_string(), Some("host".to_string()))));
        assert!(envs.contains(&("TMPDIR".to_string(), Some("/tmp".to_string()))));
        assert!(envs.contains(&("TMP".to_string(), Some("/tmp".to_string()))));
        assert!(envs.contains(&("TEMP".to_string(), Some("/tmp".to_string()))));
    }

    #[test]
    fn bridge_network_is_rejected_instead_of_running_as_host() {
        let error = super::validate_network_mode("bridge")
            .unwrap_err()
            .to_string();
        assert!(error.contains("bridge is not supported"));

        let error = super::validate_network_mode("invalid")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported network mode invalid"));
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
    fn source_visible_root_uses_home_when_source_is_under_home() {
        assert_eq!(
            super::source_visible_root_with_home(
                Path::new("/Users/me/Desktop/project"),
                Some(Path::new("/Users/me"))
            ),
            PathBuf::from("/Users/me")
        );
        assert_eq!(
            super::source_visible_root_with_home(
                Path::new("/private/tmp/project"),
                Some(Path::new("/Users/me"))
            ),
            PathBuf::from("/private/tmp/project")
        );
    }

    #[test]
    fn source_view_path_maps_preserved_cwd_inside_home_shaped_view_root() {
        let view_root = PathBuf::from("/Users/me/.agentfs/envs/codex/view-root");
        let visible_root = PathBuf::from("/Users/me");

        assert_eq!(
            super::source_view_path(
                &view_root,
                &visible_root,
                Path::new("/Users/me/Desktop/project")
            )
            .unwrap(),
            PathBuf::from("/Users/me/.agentfs/envs/codex/view-root/Desktop/project")
        );
        assert_eq!(
            super::source_view_path(
                &view_root,
                &visible_root,
                Path::new("/Users/me/Desktop/project/crates")
            )
            .unwrap(),
            PathBuf::from("/Users/me/.agentfs/envs/codex/view-root/Desktop/project/crates")
        );

        let error = super::source_view_path(&view_root, &visible_root, Path::new("/Users/other"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must be inside visible-root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn direct_mount_sandbox_writes_only_private_view_root() {
        let profile = super::direct_mount_sandbox_profile(
            Path::new("/Users/me/.agentfs/envs/codex/view-root"),
            "host",
        );

        assert!(profile.contains(
            r#"(allow file-write* (subpath "/Users/me/.agentfs/envs/codex/view-root"))"#
        ));
        assert!(!profile.contains(r#"(subpath "/Users/me/project")"#));
    }

    #[test]
    fn macos_system_fallback_roots_do_not_include_broad_user_tool_trees() {
        let roots = super::system_fallback_root_candidates();

        assert!(!roots.contains(&"/dev"));
        assert!(!roots.contains(&"/opt"));
        assert!(!roots.contains(&"/private"));
        assert!(!roots.contains(&"/usr"));
        assert!(!roots.contains(&"/usr/local"));
        assert!(!roots.contains(&"/Library"));
        assert!(!roots.contains(&"/Library/Application Support"));
        assert!(!roots.contains(&"/etc"));
        assert!(!roots.contains(&"/private/etc"));
        assert!(!roots.contains(&"/private/etc/ssh"));
        assert!(!roots.contains(&"/var"));
        assert!(roots.contains(&"/dev/null"));
        assert!(roots.contains(&"/dev/random"));
        assert!(roots.contains(&"/dev/urandom"));
        assert!(roots.contains(&"/usr/bin"));
        assert!(roots.contains(&"/usr/lib"));
        assert!(roots.contains(&"/Library/Filesystems"));
        assert!(roots.contains(&"/etc/hosts"));
        assert!(roots.contains(&"/etc/zshrc"));
        assert!(roots.contains(&"/private/etc/hosts"));
        assert!(roots.contains(&"/private/etc/zshrc"));
        assert!(roots.contains(&"/private/tmp"));
        assert!(roots.contains(&"/private/var/tmp"));
    }

    #[test]
    fn view_runtime_requires_macos_system_paths_and_requested_program() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        write_executable(view_root.join("bin/zsh"));

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
    fn view_runtime_requires_macos_dynamic_loader() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        write_executable(view_root.join("bin/zsh"));

        fs::remove_file(view_root.join("usr/lib/dyld")).unwrap();
        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "host",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/usr/lib/dyld"));
    }

    #[test]
    fn view_runtime_requires_sandbox_exec_for_network_none() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        write_executable(view_root.join("bin/zsh"));

        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "none",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/usr/bin/sandbox-exec"));

        write_executable(view_root.join("usr/bin/sandbox-exec"));
        super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "none",
        )
        .unwrap();
    }

    #[test]
    fn view_runtime_rejects_non_executable_program() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        fs::create_dir_all(view_root.join("Users/me/project")).unwrap();
        fs::write(view_root.join("bin/zsh"), "").unwrap();

        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "host",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("non-executable"));
        assert!(error.contains("/bin/zsh"));
    }

    #[test]
    fn view_runtime_requires_cwd_directory() {
        let temp = tempfile::tempdir().unwrap();
        let view_root = temp.path();
        seed_required_view_paths(view_root);
        write_executable(view_root.join("bin/zsh"));
        fs::create_dir_all(view_root.join("Users/me")).unwrap();
        fs::write(view_root.join("Users/me/project"), "").unwrap();

        let error = super::validate_view_runtime(
            view_root,
            &PathBuf::from("/Users/me/project"),
            "/bin/zsh",
            "host",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("required chroot directory"));
        assert!(error.contains("/Users/me/project"));
    }

    #[test]
    fn overlay_paths_must_match_agentfs_env_layout() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonical_temp_root(&temp);
        let env_dir = root.join("agentfs/envs/codex-1");
        let view_root = env_dir.join("view-root");
        let lower = env_dir.join("lower");
        let upper = env_dir.join("upper");
        let whiteouts = env_dir.join("whiteouts");

        super::validate_overlay_paths(&view_root, &lower, &upper, &whiteouts).unwrap();

        let error = super::validate_overlay_paths(
            &view_root,
            &root.join("other/lower"),
            &upper,
            &whiteouts,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("lower must be a sibling"));

        let error =
            super::validate_overlay_paths(&env_dir.join("rootfs"), &lower, &upper, &whiteouts)
                .unwrap_err()
                .to_string();
        assert!(error.contains("view-root must be named view-root"));
    }

    #[test]
    fn overlay_paths_reject_dot_components_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonical_temp_root(&temp);
        let env_dir = root.join("agentfs/envs/codex-1");
        fs::create_dir_all(&env_dir).unwrap();
        let lower = env_dir.join("lower");
        let upper = env_dir.join("upper");
        let whiteouts = env_dir.join("whiteouts");

        let error = super::validate_overlay_paths(
            &env_dir.join("../codex-1/view-root"),
            &lower,
            &upper,
            &whiteouts,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must not contain . or .."));

        let real_envs = root.join("real-envs");
        fs::create_dir_all(&real_envs).unwrap();
        let linked_envs = root.join("agentfs/envs-link");
        std::os::unix::fs::symlink(&real_envs, &linked_envs).unwrap();
        let linked_env_dir = linked_envs.join("codex-1");
        let error = super::validate_overlay_paths(
            &linked_env_dir.join("view-root"),
            &linked_env_dir.join("lower"),
            &linked_env_dir.join("upper"),
            &linked_env_dir.join("whiteouts"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("symlink component"));
    }

    #[test]
    fn session_log_path_must_stay_inside_env_session_logs() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonical_temp_root(&temp);
        let env_dir = root.join("agentfs/envs/codex-1");
        let view_root = env_dir.join("view-root");
        let log_path = env_dir.join("logs/sessions/dev.log");

        super::validate_session_log_path(&view_root, &log_path).unwrap();

        let error = super::validate_session_log_path(&view_root, &env_dir.join("logs/dev.log"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("log-path must be inside"));

        let error =
            super::validate_session_log_path(&view_root, &env_dir.join("logs/sessions/../dev.log"))
                .unwrap_err()
                .to_string();
        assert!(error.contains("must not contain . or .."));
    }

    #[test]
    fn session_log_path_rejects_symlink_components() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonical_temp_root(&temp);
        let env_dir = root.join("agentfs/envs/codex-1");
        let view_root = env_dir.join("view-root");
        let real_logs = root.join("real-logs");
        fs::create_dir_all(&real_logs).unwrap();
        fs::create_dir_all(env_dir.join("logs")).unwrap();
        std::os::unix::fs::symlink(&real_logs, env_dir.join("logs/sessions")).unwrap();

        let error =
            super::validate_session_log_path(&view_root, &env_dir.join("logs/sessions/dev.log"))
                .unwrap_err()
                .to_string();
        assert!(error.contains("symlink component"));
    }

    fn seed_required_view_paths(view_root: &std::path::Path) {
        for path in [
            "bin", "usr/bin", "usr/lib", "System", "Library", "private", "dev",
        ] {
            fs::create_dir_all(view_root.join(path)).unwrap();
        }
        fs::write(view_root.join("dev/null"), "").unwrap();
        fs::write(view_root.join("usr/lib/dyld"), "").unwrap();
        write_executable(view_root.join("usr/bin/env"));
    }

    fn write_executable(path: PathBuf) {
        fs::write(&path, "").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
