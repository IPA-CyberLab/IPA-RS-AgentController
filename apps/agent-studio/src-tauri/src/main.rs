use agent_core::config::{default_agentfs, AgentConfig};
use agent_core::model::{Env, EnvStatus, LimitOverrides, RootfsBackend};
use agent_core::protocol::{Request, Response};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

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
struct PickFolderInput {
    default_path: Option<PathBuf>,
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
struct ExecInput {
    env_id: String,
    command: String,
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
fn pick_source_root(
    _options: RuntimeOptions,
    input: PickFolderInput,
) -> Result<serde_json::Value, String> {
    let path = pick_folder(input.default_path.as_deref()).map_err(error_string)?;
    Ok(json!({ "path": path }))
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
fn changed_paths(options: RuntimeOptions, input: EnvInput) -> Result<Value, String> {
    call_value(
        &options,
        Request::Export {
            env_id: input.env_id,
            export_type: "rootfs-changed-paths".to_string(),
        },
    )
}

#[tauri::command]
fn run_command(options: RuntimeOptions, input: ExecInput) -> Result<Value, String> {
    call_value(
        &options,
        Request::Exec {
            id: input.env_id,
            command: shell_command(&input.command),
            cwd: None,
        },
    )
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
                let env = read_env_metadata(&path)?;
                envs.push(EnvStatus {
                    env,
                    disk_used: None,
                });
            }
        }
    }
    envs.sort_by(|a, b| a.env.id.cmp(&b.env.id));
    Ok(serde_json::to_value(Response::Envs { envs })?)
}

fn env_status_from_metadata(config: &AgentConfig, env_id: &str) -> Result<Value> {
    let env = read_env_metadata(&config.agentfs.join("envs").join(env_id).join("meta.json"))?;
    Ok(serde_json::to_value(Response::EnvStatus {
        status: Box::new(EnvStatus {
            env,
            disk_used: None,
        }),
    })?)
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

fn shell_command(command: &str) -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        vec!["cmd.exe".to_string(), "/C".to_string(), command.to_string()]
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            command.to_string(),
        ]
    }
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
fn pick_folder(default_path: Option<&Path>) -> Result<Option<PathBuf>> {
    let mut script = String::from("POSIX path of (choose folder with prompt \"Open Folder\"");
    if let Some(default_path) = default_path.filter(|path| path.exists()) {
        script.push_str(" default location POSIX file ");
        script.push_str(&apple_script_string(&default_path.display().to_string()));
    }
    script.push(')');
    folder_output(Command::new("osascript").arg("-e").arg(script).output())
}

#[cfg(target_os = "windows")]
fn pick_folder(default_path: Option<&Path>) -> Result<Option<PathBuf>> {
    let selected_path = default_path
        .filter(|path| path.exists())
        .map(|path| powershell_string(&path.display().to_string()))
        .unwrap_or_else(|| "''".to_string());
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = 'Open Folder'
if ({selected_path}.Length -gt 0) {{ $dialog.SelectedPath = {selected_path} }}
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{
  [Console]::Out.Write($dialog.SelectedPath)
}}
"#
    );
    folder_output(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-STA", "-Command", &script])
            .output(),
    )
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn pick_folder(default_path: Option<&Path>) -> Result<Option<PathBuf>> {
    let mut zenity = Command::new("zenity");
    zenity.args(["--file-selection", "--directory", "--title", "Open Folder"]);
    if let Some(default_path) = default_path.filter(|path| path.exists()) {
        zenity.arg("--filename").arg(default_path);
    }
    match folder_output(zenity.output()) {
        Ok(path) => Ok(path),
        Err(zenity_error) => {
            let mut kdialog = Command::new("kdialog");
            kdialog.arg("--getexistingdirectory");
            if let Some(default_path) = default_path.filter(|path| path.exists()) {
                kdialog.arg(default_path);
            }
            folder_output(kdialog.output()).with_context(|| {
                format!(
                    "failed to open folder picker with zenity or kdialog; zenity error: {zenity_error}"
                )
            })
        }
    }
}

fn folder_output(output: std::io::Result<std::process::Output>) -> Result<Option<PathBuf>> {
    let output = output.context("failed to start folder picker")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let value = stdout.trim();
        return Ok((!value.is_empty()).then(|| PathBuf::from(value)));
    }
    let message = stderr.trim();
    if message.contains("-128")
        || message.contains("User canceled")
        || message.contains("cancel")
        || output.status.code() == Some(1)
    {
        return Ok(None);
    }
    bail!("folder picker failed: {message}");
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "windows")]
fn powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
            pick_source_root,
            list_envs,
            create_lane,
            remove_lane,
            changed_paths,
            run_command,
            open_ide,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Agent Studio");
}
