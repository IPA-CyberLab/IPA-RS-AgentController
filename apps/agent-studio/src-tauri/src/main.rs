use agent_core::config::{default_agentfs, AgentConfig};
use agent_core::model::{Base, Env, LimitOverrides, RootfsBackend};
use agent_core::protocol::{Request, Response};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Deserialize)]
struct RuntimeOptions {
    agentfs: Option<PathBuf>,
    config: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct StudioConfig {
    agentfs: PathBuf,
    default_source: Option<PathBuf>,
    platform: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
struct CreateLaneInput {
    target: String,
    source: PathBuf,
    backend: Option<String>,
    profile: Option<String>,
    network: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct EnvInput {
    env_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenIdeInput {
    env_id: String,
    app: String,
    relative_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PickFolderInput {
    initial: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct PathResponse {
    path: Option<PathBuf>,
}

#[tauri::command]
fn app_config(options: RuntimeOptions) -> Result<StudioConfig, String> {
    let config = load_config(&options).map_err(error_string)?;
    Ok(StudioConfig {
        agentfs: config.agentfs,
        default_source: std::env::var_os("AGENT_STUDIO_SOURCE").map(PathBuf::from),
        platform: platform_name(),
    })
}

#[tauri::command]
fn list_envs(options: RuntimeOptions) -> Result<Value, String> {
    let config = load_config(&options).map_err(error_string)?;
    envs_from_metadata(&config).map_err(error_string)
}

#[tauri::command]
fn create_lane(options: RuntimeOptions, input: CreateLaneInput) -> Result<Value, String> {
    let config = load_config(&options).map_err(error_string)?;
    let source = normalize_source(&input.source).map_err(error_string)?;
    let backend = backend_from_ui(input.backend.as_deref()).map_err(error_string)?;
    let base = default_base_for_source(&source);
    let profile = input
        .profile
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.default_profile.clone());
    let limits = LimitOverrides {
        network: input.network.filter(|value| !value.trim().is_empty()),
        ..LimitOverrides::default()
    };
    ok_or_already_exists(call_control(
        &config,
        Request::BaseFreeze {
            name: base.clone(),
            from: source,
            backend,
        },
    ))
    .map_err(error_string)?;
    ok_or_already_exists(call_control(
        &config,
        Request::EnvCreate {
            id: input.target.clone(),
            base,
            profile,
            limits,
        },
    ))
    .map_err(error_string)?;
    ok_response(
        call_control(
            &config,
            Request::EnvStart {
                id: input.target.clone(),
            },
        )
        .map_err(error_string)?,
    )
    .map_err(error_string)?;
    env_status_from_metadata(&config, &input.target).map_err(error_string)
}

#[tauri::command]
fn remove_lane(options: RuntimeOptions, input: EnvInput) -> Result<Value, String> {
    call_value(&options, Request::EnvDestroy { id: input.env_id })
}

#[tauri::command]
fn pick_source_root(input: PickFolderInput) -> Result<PathResponse, String> {
    pick_folder(input.initial.as_deref())
        .map(|path| PathResponse { path })
        .map_err(error_string)
}

#[tauri::command]
fn open_ide(options: RuntimeOptions, input: OpenIdeInput) -> Result<Value, String> {
    let config = load_config(&options).map_err(error_string)?;
    let rootfs = env_rootfs_from_metadata(&config, &input.env_id).map_err(error_string)?;
    let relative = input.relative_path.unwrap_or_default();
    let path = rootfs.join(safe_relative_path(&relative).map_err(error_string)?);
    launch_known_app(&input.app, &path).map_err(error_string)?;
    Ok(json!({ "path": path }))
}

#[tauri::command]
fn open_shell(options: RuntimeOptions, input: EnvInput) -> Result<Value, String> {
    let config = load_config(&options).map_err(error_string)?;
    let env = read_env_metadata(
        &config
            .agentfs
            .join("envs")
            .join(&input.env_id)
            .join("meta.json"),
    )
    .map_err(error_string)?;
    let source_root = read_base_source(&config, &env.base_id)
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let command = agentctl_shell_command(&options, &config, &env.id, source_root.as_deref());
    launch_native_terminal(&command).map_err(error_string)?;
    Ok(json!({ "opened": true, "command": command }))
}

fn call_value(options: &RuntimeOptions, request: Request) -> Result<Value, String> {
    let config = load_config(options).map_err(error_string)?;
    call_value_with_config(&config, request).map_err(error_string)
}

fn call_value_with_config(config: &AgentConfig, request: Request) -> Result<Value> {
    let response = call_control(config, request)?;
    Ok(serde_json::to_value(ok_response(response)?)?)
}

fn ok_response(response: Response) -> Result<Response> {
    match response {
        Response::Error { message } => bail!(message),
        other => Ok(other),
    }
}

fn ok_or_already_exists(response: Result<Response>) -> Result<()> {
    match response? {
        Response::Ok => Ok(()),
        Response::Error { message } if message.contains("already exists") => Ok(()),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected create response: {other:?}"),
    }
}

fn envs_from_metadata(config: &AgentConfig) -> Result<Value> {
    let dir = config.agentfs.join("envs");
    let mut envs = Vec::new();
    if dir.exists() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path().join("meta.json");
            if path.exists() {
                envs.push(env_status_value(config, read_env_metadata(&path)?)?);
            }
        }
    }
    envs.sort_by(|a, b| {
        let a = a
            .get("env")
            .and_then(|env| env.get("id"))
            .and_then(Value::as_str);
        let b = b
            .get("env")
            .and_then(|env| env.get("id"))
            .and_then(Value::as_str);
        a.cmp(&b)
    });
    Ok(json!({ "type": "envs", "envs": envs }))
}

fn env_status_from_metadata(config: &AgentConfig, env_id: &str) -> Result<Value> {
    let env = read_env_metadata(&config.agentfs.join("envs").join(env_id).join("meta.json"))?;
    Ok(json!({ "type": "env_status", "status": env_status_value(config, env)? }))
}

fn env_rootfs_from_metadata(config: &AgentConfig, env_id: &str) -> Result<PathBuf> {
    let env = read_env_metadata(&config.agentfs.join("envs").join(env_id).join("meta.json"))?;
    Ok(env.rootfs_path)
}

fn read_env_metadata(path: &Path) -> Result<Env> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid env metadata {}", path.display()))
}

fn env_status_value(config: &AgentConfig, env: Env) -> Result<Value> {
    let source_root = read_base_source(config, &env.base_id).ok();
    let env_path = config.agentfs.join("envs").join(&env.id);
    Ok(json!({
        "env": env,
        "disk_used": null,
        "source_root": source_root,
        "env_path": env_path,
    }))
}

fn read_base_source(config: &AgentConfig, base_id: &str) -> Result<String> {
    let path = config
        .agentfs
        .join("bases")
        .join(base_id)
        .join("manifest.json");
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let base: Base = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid base metadata {}", path.display()))?;
    Ok(base.source)
}

fn load_config(options: &RuntimeOptions) -> Result<AgentConfig> {
    if let Some(path) = &options.config {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: AgentConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid config json {}", path.display()))?;
        config.validate()?;
        return Ok(config);
    }
    Ok(AgentConfig::new(
        options.agentfs.clone().unwrap_or_else(default_agentfs),
    ))
}

#[cfg(target_os = "linux")]
fn call_control(config: &AgentConfig, request: Request) -> Result<Response> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(&config.socket_path)
        .with_context(|| format!("failed to connect {}", config.socket_path.display()))?;
    send_request(&mut stream, request)?;
    read_response(config.socket_path.display().to_string(), stream)
}

#[cfg(not(target_os = "linux"))]
fn call_control(config: &AgentConfig, request: Request) -> Result<Response> {
    let mut stream = std::net::TcpStream::connect(&config.tcp_addr)
        .with_context(|| format!("failed to connect {}", config.tcp_addr))?;
    send_request(&mut stream, request)?;
    read_response(config.tcp_addr.clone(), stream)
}

fn send_request<W: Write>(stream: &mut W, request: Request) -> Result<()> {
    let bytes = serde_json::to_vec(&request)?;
    stream.write_all(&bytes)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn read_response<R: std::io::Read>(source: String, stream: R) -> Result<Response> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("agent-forkd closed {source} without a response");
    }
    agent_core::protocol::parse_response_json(&line).map_err(|error| {
        anyhow!(
            "invalid response json from {source}: {error}: {}",
            line.trim_end()
        )
    })
}

fn default_base_for_source(source: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = source;
        "base-001".to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in source.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("base-{hash:016x}")
    }
}

fn backend_from_ui(value: Option<&str>) -> Result<Option<RootfsBackend>> {
    Ok(match value.unwrap_or("default") {
        "" | "default" => None,
        "apfs-clone" => Some(RootfsBackend::ApfsClone),
        "windows-block-clone" => Some(RootfsBackend::WindowsBlockClone),
        "path-preserving-overlay" => Some(RootfsBackend::PathPreservingOverlay),
        "windows-minifilter-overlay" => Some(RootfsBackend::WindowsMinifilterOverlay),
        other => bail!("unknown backend {other}"),
    })
}

fn normalize_source(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path must be relative and stay inside the env rootfs: {value}");
            }
        }
    }
    Ok(path)
}

fn launch_known_app(app: &str, path: &Path) -> Result<()> {
    match app {
        "code" => spawn_detached("code", &[path.to_path_buf()]),
        "code-insiders" => spawn_detached("code-insiders", &[path.to_path_buf()]),
        "cursor" => spawn_detached("cursor", &[path.to_path_buf()]),
        "zed" => spawn_detached("zed", &[path.to_path_buf()]),
        "reveal" => reveal_path(path),
        other => bail!("unknown app {other}"),
    }
}

fn reveal_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        spawn_detached("open", &[path.to_path_buf()])
    }
    #[cfg(target_os = "windows")]
    {
        spawn_detached("explorer", &[path.to_path_buf()])
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        spawn_detached("xdg-open", &[path.to_path_buf()])
    }
}

fn spawn_detached(program: &str, args: &[PathBuf]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    command
        .spawn()
        .with_context(|| format!("failed to launch {program}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn pick_folder(initial: Option<&Path>) -> Result<Option<PathBuf>> {
    let script = if let Some(path) = initial.filter(|path| path.exists()) {
        format!(
            "set defaultFolder to POSIX file {} as alias\nPOSIX path of (choose folder with prompt \"Choose root folder\" default location defaultFolder)",
            apple_script_string(&path.display().to_string())
        )
    } else {
        "POSIX path of (choose folder with prompt \"Choose root folder\")".to_string()
    };
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("failed to open folder picker")?;
    folder_output(output, &["User canceled"])
}

#[cfg(target_os = "windows")]
fn pick_folder(initial: Option<&Path>) -> Result<Option<PathBuf>> {
    let selected_path = initial
        .filter(|path| path.exists())
        .map(|path| {
            format!(
                "if (Test-Path -LiteralPath {}) {{ $dialog.SelectedPath = {} }};",
                powershell_string(&path.display().to_string()),
                powershell_string(&path.display().to_string())
            )
        })
        .unwrap_or_default();
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $dialog = New-Object System.Windows.Forms.FolderBrowserDialog; \
         $dialog.Description = 'Choose root folder'; \
         $dialog.ShowNewFolderButton = $false; \
         {selected_path} \
         if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ \
           [Console]::Out.WriteLine($dialog.SelectedPath) \
         }}"
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", &script])
        .output()
        .context("failed to open folder picker")?;
    folder_output(output, &[])
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn pick_folder(initial: Option<&Path>) -> Result<Option<PathBuf>> {
    let initial = initial
        .filter(|path| path.exists())
        .map(|path| path.display().to_string());
    let zenity_args = if let Some(path) = &initial {
        vec![
            "--file-selection",
            "--directory",
            "--filename",
            path.as_str(),
        ]
    } else {
        vec!["--file-selection", "--directory"]
    };
    let kdialog_args = if let Some(path) = &initial {
        vec!["--getexistingdirectory", path.as_str()]
    } else {
        vec!["--getexistingdirectory"]
    };
    for (program, args) in [("zenity", zenity_args), ("kdialog", kdialog_args)] {
        match Command::new(program).args(args).output() {
            Ok(output) => return folder_output(output, &["No file selected"]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("failed to run {program}")),
        }
    }
    bail!("failed to find a folder picker command");
}

fn folder_output(output: Output, cancel_markers: &[&str]) -> Result<Option<PathBuf>> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let selected = stdout.trim();
    if output.status.success() {
        return Ok((!selected.is_empty()).then(|| PathBuf::from(selected)));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if cancel_markers
        .iter()
        .any(|marker| message.contains(marker) || selected.contains(marker))
    {
        return Ok(None);
    }
    bail!(
        "{}",
        if message.is_empty() {
            format!("folder picker exited with {}", output.status)
        } else {
            message.to_string()
        }
    )
}

fn agentctl_shell_command(
    options: &RuntimeOptions,
    config: &AgentConfig,
    env_id: &str,
    source_root: Option<&Path>,
) -> String {
    let mut parts = Vec::new();
    if let Some(source_root) = source_root {
        parts.push("cd".to_string());
        parts.push(shell_quote(&source_root.display().to_string()));
        parts.push("&&".to_string());
    }
    parts.push(shell_quote(&agentctl_program().display().to_string()));
    if let Some(config_path) = &options.config {
        parts.push("--config".to_string());
        parts.push(shell_quote(&config_path.display().to_string()));
    } else {
        parts.push("--agentfs".to_string());
        parts.push(shell_quote(&config.agentfs.display().to_string()));
    }
    parts.push("shell".to_string());
    parts.push(shell_quote(env_id));
    parts.join(" ")
}

fn agentctl_program() -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_STUDIO_AGENTCTL") {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/agentctl");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("agentctl")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
fn powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "macos")]
fn launch_native_terminal(command: &str) -> Result<()> {
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script {}\nend tell",
        apple_script_string(command)
    );
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .context("failed to launch Terminal.app")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_native_terminal(command: &str) -> Result<()> {
    Command::new("cmd.exe")
        .args(["/C", "start", "", "cmd.exe", "/K", command])
        .spawn()
        .context("failed to launch cmd.exe")?;
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn launch_native_terminal(command: &str) -> Result<()> {
    let command = format!("{command}; exec ${{SHELL:-/bin/sh}}");
    let candidates: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e", "sh", "-lc"]),
        ("gnome-terminal", &["--", "sh", "-lc"]),
        ("konsole", &["-e", "sh", "-lc"]),
        ("xterm", &["-e", "sh", "-lc"]),
    ];
    for (program, args) in candidates {
        let mut child = Command::new(program);
        child.args(*args).arg(&command);
        if child.spawn().is_ok() {
            return Ok(());
        }
    }
    bail!("failed to find a terminal emulator for agentctl shell");
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn platform_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        "linux"
    }
}

fn error_string(error: anyhow::Error) -> String {
    error.to_string()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            app_config,
            list_envs,
            create_lane,
            remove_lane,
            pick_source_root,
            open_ide,
            open_shell,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Agent Studio");
}
