use crate::{call_control, default_base_for_source, StudioArgs};
use agent_core::config::AgentConfig;
use agent_core::model::{Env, EnvStatus, LimitOverrides, RootfsBackend};
use agent_core::protocol::{Request, Response};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::process::Command as StdCommand;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub(crate) async fn run(config: AgentConfig, args: StudioArgs) -> Result<()> {
    let addr: SocketAddr = args
        .addr
        .parse()
        .with_context(|| format!("invalid studio addr {}", args.addr))?;
    let source = normalize_source_path(&args.source)?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind studio at {addr}"))?;
    let bound = listener.local_addr()?;
    let url = format!("http://{bound}/");

    println!("agentctl studio listening on {url}");
    println!("source root: {}", source.display());
    if !args.no_open {
        if let Err(error) = open_browser(&url) {
            eprintln!("warning: failed to open browser: {error:#}");
        }
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let config = config.clone();
        let source = source.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, config, source).await {
                eprintln!("studio request failed: {error:#}");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    config: AgentConfig,
    default_source: PathBuf,
) -> Result<()> {
    let request = match read_http_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            write_response(&mut stream, http_json(400, json_error(error))).await?;
            return Ok(());
        }
    };
    let response = route(request, &config, &default_source).await;
    let response = match response {
        Ok(response) => response,
        Err(error) => http_json(500, json_error(error)),
    };
    write_response(&mut stream, response).await
}

async fn route(
    request: HttpRequest,
    config: &AgentConfig,
    default_source: &Path,
) -> Result<HttpResponse> {
    let path = request.path.split('?').next().unwrap_or("/");
    match (request.method.as_str(), path) {
        ("GET", "/") => Ok(http_html(INDEX_HTML.replace(
            "__DEFAULT_SOURCE__",
            &html_escape(&default_source.display().to_string()),
        ))),
        ("GET", "/api/config") => Ok(http_json(
            200,
            json!({
                "ok": true,
                "data": {
                    "agentfs": config.agentfs,
                    "default_source": default_source,
                    "platform": platform_name(),
                }
            }),
        )),
        ("GET", "/api/envs") => Ok(http_json(200, ok_value(envs_from_metadata(config)?))),
        ("POST", "/api/new") => {
            let body = json_body(&request)?;
            let target = required_string(&body, "target")?;
            let source = body
                .get("source")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| default_source.to_path_buf());
            let source = normalize_source_path_against(&source, default_source)?;
            let backend = optional_backend(&body)?;
            let network = optional_string(&body, "network");
            let profile = optional_string(&body, "profile");
            let base = default_base_for_source(&source);
            ok_or_already_exists(
                call_control(
                    config,
                    Request::BaseFreeze {
                        name: base.clone(),
                        from: source,
                        backend,
                    },
                )
                .await?,
            )?;
            ok_or_already_exists(
                call_control(
                    config,
                    Request::EnvCreate {
                        id: target.clone(),
                        base,
                        profile: profile.unwrap_or_else(|| config.default_profile.clone()),
                        limits: LimitOverrides {
                            network,
                            ..LimitOverrides::default()
                        },
                    },
                )
                .await?,
            )?;
            let response = call_control(config, Request::EnvStart { id: target.clone() }).await?;
            ok_response_value(response)?;
            Ok(http_json(
                200,
                ok_value(env_status_from_metadata(config, &target)?),
            ))
        }
        ("POST", "/api/remove") => {
            let body = json_body(&request)?;
            let env_id = required_string(&body, "env_id")?;
            let response = call_control(config, Request::EnvDestroy { id: env_id }).await?;
            Ok(http_json(200, ok_response(response)))
        }
        ("POST", "/api/export") => {
            let body = json_body(&request)?;
            let env_id = required_string(&body, "env_id")?;
            let response = call_control(
                config,
                Request::Export {
                    env_id,
                    export_type: "rootfs-changed-paths".to_string(),
                },
            )
            .await?;
            Ok(http_json(200, ok_response(response)))
        }
        ("POST", "/api/exec") => {
            let body = json_body(&request)?;
            let env_id = required_string(&body, "env_id")?;
            let command = required_string(&body, "command")?;
            let response = call_control(
                config,
                Request::Exec {
                    id: env_id,
                    command: shell_command(&command),
                    cwd: None,
                },
            )
            .await?;
            Ok(http_json(200, ok_response(response)))
        }
        ("POST", "/api/open") => {
            let body = json_body(&request)?;
            let env_id = required_string(&body, "env_id")?;
            let app = required_string(&body, "app")?;
            let relative = optional_string(&body, "relative_path").unwrap_or_default();
            let rootfs = env_rootfs_from_metadata(config, &env_id)?;
            let target = rootfs.join(safe_relative_path(&relative)?);
            launch_known_app(&app, &target)?;
            Ok(http_json(
                200,
                json!({"ok": true, "data": {"launched": app, "path": target}}),
            ))
        }
        ("POST", "/api/launch") => {
            let body = json_body(&request)?;
            let env_id = required_string(&body, "env_id")?;
            let program = required_string(&body, "program")?;
            let args = body
                .get("args")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let rootfs = env_rootfs_from_metadata(config, &env_id)?;
            let program = expand_root_placeholder(&program, &rootfs);
            let args = args
                .into_iter()
                .map(|arg| expand_root_placeholder(&arg, &rootfs))
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            launch_program(Path::new(&program), &args)?;
            Ok(http_json(
                200,
                json!({"ok": true, "data": {"launched": program}}),
            ))
        }
        _ => Ok(http_json(
            404,
            json!({"ok": false, "error": format!("unknown route {} {}", request.method, path)}),
        )),
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
                envs.push(EnvStatus {
                    env: read_env_metadata(&path)?,
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

fn launch_program(program: &Path, args: &[PathBuf]) -> Result<()> {
    let mut command = StdCommand::new(program);
    command.args(args);
    command
        .spawn()
        .with_context(|| format!("failed to launch {}", program.display()))?;
    Ok(())
}

fn spawn_detached(program: &str, args: &[PathBuf]) -> Result<()> {
    let mut command = StdCommand::new(program);
    command.args(args);
    command
        .spawn()
        .with_context(|| format!("failed to launch {program}"))?;
    Ok(())
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

fn optional_backend(body: &Value) -> Result<Option<RootfsBackend>> {
    let Some(raw) = optional_string(body, "backend") else {
        return Ok(None);
    };
    if raw == "default" {
        return Ok(None);
    }
    Ok(Some(match raw.as_str() {
        "apfs-clone" => RootfsBackend::ApfsClone,
        "windows-block-clone" => RootfsBackend::WindowsBlockClone,
        "path-preserving-overlay" => RootfsBackend::PathPreservingOverlay,
        "windows-minifilter-overlay" => RootfsBackend::WindowsMinifilterOverlay,
        other => bail!("unknown backend {other}"),
    }))
}

fn required_string(body: &Value, key: &str) -> Result<String> {
    optional_string(body, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("missing string field {key}"))
}

fn optional_string(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn normalize_source_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalize_source_path_against(path: &Path, default_source: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(default_source.join(path))
    }
}

fn expand_root_placeholder(value: &str, rootfs: &Path) -> String {
    value.replace("{root}", &rootfs.display().to_string())
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

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        StdCommand::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        StdCommand::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        StdCommand::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

fn json_body(request: &HttpRequest) -> Result<Value> {
    if request.body.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice(&request.body).context("invalid json body")
}

fn ok_response(response: Response) -> Value {
    match response {
        Response::Error { message } => json!({"ok": false, "error": message}),
        other => json!({"ok": true, "data": other}),
    }
}

fn ok_response_value(response: Response) -> Result<Value> {
    match response {
        Response::Error { message } => bail!(message),
        other => Ok(serde_json::to_value(other)?),
    }
}

fn ok_value(value: Value) -> Value {
    json!({"ok": true, "data": value})
}

fn ok_or_already_exists(response: Response) -> Result<()> {
    match response {
        Response::Ok => Ok(()),
        Response::Error { message } if message.contains("already exists") => Ok(()),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected create response: {other:?}"),
    }
}

fn json_error(error: anyhow::Error) -> Value {
    json!({"ok": false, "error": error.to_string()})
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let header_end = loop {
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before request headers");
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let header = std::str::from_utf8(&buffer[..header_end]).context("headers are not utf-8")?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing method"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing path"))?
        .to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before request body");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[body_start..body_start + content_length].to_vec();
    Ok(HttpRequest { method, path, body })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    let status_text = match response.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        response.status,
        status_text,
        response.content_type,
        response.body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    Ok(())
}

fn http_json(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        content_type: "application/json; charset=utf-8",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec()),
    }
}

fn http_html(body: String) -> HttpResponse {
    HttpResponse {
        status: 200,
        content_type: "text/html; charset=utf-8",
        body: body.into_bytes(),
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Agent Studio</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f7f7f4;
      --panel: #ffffff;
      --line: #d8d9d2;
      --ink: #222522;
      --muted: #676d66;
      --green: #1f7a4d;
      --blue: #275e91;
      --amber: #966820;
      --danger: #9a3b38;
      --soft: #edf1ed;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background: var(--bg);
      color: var(--ink);
      font: 14px/1.45 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    header {
      height: 56px;
      display: flex;
      align-items: center;
      gap: 18px;
      padding: 0 20px;
      border-bottom: 1px solid var(--line);
      background: #fbfbf8;
    }
    h1 {
      margin: 0;
      font-size: 18px;
      font-weight: 680;
      letter-spacing: 0;
    }
    main {
      display: grid;
      grid-template-columns: 360px 1fr;
      gap: 18px;
      padding: 18px;
      min-height: calc(100vh - 56px);
    }
    aside, section {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
    }
    aside {
      padding: 16px;
      align-self: start;
    }
    section {
      min-width: 0;
      overflow: hidden;
    }
    h2 {
      margin: 0 0 12px;
      font-size: 14px;
      font-weight: 700;
      color: #2b332b;
    }
    label {
      display: block;
      margin: 12px 0 5px;
      color: var(--muted);
      font-size: 12px;
      font-weight: 650;
    }
    input, select, textarea {
      width: 100%;
      border: 1px solid #c9cbc4;
      border-radius: 6px;
      background: #fff;
      color: var(--ink);
      padding: 8px 9px;
      font: inherit;
      min-height: 36px;
    }
    textarea {
      min-height: 78px;
      resize: vertical;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    }
    button {
      min-height: 34px;
      border: 1px solid #b8bcb4;
      border-radius: 6px;
      background: #fff;
      color: var(--ink);
      padding: 6px 10px;
      font: inherit;
      font-weight: 650;
      cursor: pointer;
      white-space: nowrap;
    }
    button:hover { border-color: #879084; background: #f8faf8; }
    button.primary {
      color: #fff;
      border-color: var(--green);
      background: var(--green);
    }
    button.danger {
      color: #fff;
      border-color: var(--danger);
      background: var(--danger);
    }
    button.blue {
      color: #fff;
      border-color: var(--blue);
      background: var(--blue);
    }
    button.wide { width: 100%; }
    .rootSummary {
      margin: 12px 0;
      padding: 10px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #f7f8f4;
    }
    .rootSummary span,
    .rootSummary strong {
      display: block;
    }
    .rootSummary span {
      color: var(--muted);
      font-size: 12px;
      font-weight: 700;
    }
    .rootSummary strong {
      margin-top: 3px;
      overflow-wrap: anywhere;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
    }
    .row {
      display: flex;
      gap: 8px;
      align-items: center;
    }
    .row > * { flex: 1; }
    .toolbar {
      display: flex;
      gap: 8px;
      align-items: center;
      justify-content: space-between;
      padding: 12px;
      border-bottom: 1px solid var(--line);
      background: #fbfbf8;
    }
    .toolbar .left, .actions {
      display: flex;
      gap: 8px;
      align-items: center;
      flex-wrap: wrap;
    }
    .status {
      color: var(--muted);
      font-size: 12px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      max-width: 58vw;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      table-layout: fixed;
    }
    th, td {
      padding: 10px 12px;
      border-bottom: 1px solid var(--line);
      text-align: left;
      vertical-align: top;
    }
    th {
      background: #f2f3ef;
      color: #4f564e;
      font-size: 12px;
      font-weight: 750;
    }
    td {
      overflow-wrap: anywhere;
    }
    .env { font-weight: 750; }
    .pill {
      display: inline-flex;
      align-items: center;
      height: 22px;
      padding: 0 8px;
      border: 1px solid var(--line);
      border-radius: 999px;
      background: var(--soft);
      color: #3f473f;
      font-size: 12px;
      font-weight: 700;
    }
    .pill.running { border-color: #b5d5c4; background: #e6f4ec; color: #17653b; }
    .pill.failed, .pill.quota_exceeded { border-color: #e5b9b6; background: #fff0ef; color: #8f302d; }
    .muted { color: var(--muted); }
    .mono {
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
    }
    pre {
      margin: 0;
      max-height: 220px;
      overflow: auto;
      border-top: 1px solid var(--line);
      background: #202520;
      color: #edf4ec;
      padding: 12px;
      font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    }
    .split {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 8px;
    }
    @media (max-width: 980px) {
      main { grid-template-columns: 1fr; }
      .status { max-width: 100%; }
      table { min-width: 860px; }
      section { overflow-x: auto; }
    }
  </style>
</head>
<body>
  <header>
    <h1>Agent Studio</h1>
    <div id="meta" class="status">Loading</div>
  </header>
  <main>
    <aside>
      <h2>New World</h2>
      <button id="openFolder" class="primary wide" onclick="chooseSource()">Open Folder</button>
      <div id="worldForm" style="display:none">
        <div class="rootSummary">
          <span>Root</span>
          <strong id="sourceLabel"></strong>
        </div>
        <label for="target">Name</label>
        <input id="target" value="" autocomplete="off">
        <p><button class="primary wide" onclick="createLane()">Create</button></p>
      </div>
    </aside>
    <section>
      <div class="toolbar">
        <div class="left">
          <button onclick="refresh()">Refresh</button>
          <button onclick="clearOutput()">Clear</button>
        </div>
        <div id="status" class="status"></div>
      </div>
      <table>
        <thead>
          <tr>
            <th style="width: 16%">Env</th>
            <th style="width: 18%">Backend</th>
            <th style="width: 12%">State</th>
            <th>Rootfs</th>
            <th style="width: 28%">Actions</th>
          </tr>
        </thead>
        <tbody id="envRows"></tbody>
      </table>
      <pre id="output"></pre>
    </section>
  </main>
  <script>
    const $ = id => document.getElementById(id);
    const stateClass = value => String(value || '').replaceAll('_', '-');

    async function api(path, options = {}) {
      const response = await fetch(path, {
        headers: {'content-type': 'application/json'},
        ...options
      });
      const json = await response.json();
      if (!json.ok) throw new Error(json.error || 'request failed');
      return json.data;
    }

    function body(value) {
      return {method: 'POST', body: JSON.stringify(value)};
    }

    function log(value) {
      const text = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
      $('output').textContent = text + '\n' + $('output').textContent;
    }

    function setStatus(value) {
      $('status').textContent = value;
    }

    async function refresh() {
      setStatus('Refreshing');
      try {
        const data = await api('/api/envs');
        const envs = data.envs || [];
        $('envRows').innerHTML = envs.map(item => row(item)).join('');
        setStatus(`${envs.length} worlds`);
      } catch (error) {
        setStatus(error.message);
      }
    }

    function row(item) {
      const env = item.env;
      const sessions = env.sessions && env.sessions.length ? env.sessions.join(', ') : '-';
      return `
        <tr>
          <td><div class="env">${escapeHtml(env.id)}</div><div class="muted mono">${escapeHtml(env.base_id)}</div></td>
          <td><span class="pill">${escapeHtml(env.backend)}</span><div class="muted mono">${escapeHtml(item.disk_used || '-')}</div></td>
          <td><span class="pill ${escapeHtml(env.state)}">${escapeHtml(env.state)}</span><div class="muted">${escapeHtml(sessions)}</div></td>
          <td class="mono">${escapeHtml(env.rootfs_path)}</td>
          <td>
            <div class="actions">
              <button class="blue" onclick="openApp('${escapeAttr(env.id)}','code')">VSCode</button>
              <button onclick="openApp('${escapeAttr(env.id)}','cursor')">Cursor</button>
              <button onclick="openApp('${escapeAttr(env.id)}','reveal')">Folder</button>
              <button onclick="changed('${escapeAttr(env.id)}')">Changed</button>
              <button class="danger" onclick="removeLane('${escapeAttr(env.id)}')">Remove</button>
            </div>
          </td>
        </tr>`;
    }

    const defaultSource = "__DEFAULT_SOURCE__";
    let sourceRoot = "";

    function chooseSource() {
      const picked = prompt('Root folder', sourceRoot || defaultSource);
      if (!picked) return;
      sourceRoot = picked;
      $('sourceLabel').textContent = sourceRoot;
      $('target').value = suggestWorldName(sourceRoot);
      $('worldForm').style.display = '';
      $('openFolder').textContent = 'Change Folder';
      $('openFolder').classList.remove('primary');
      setStatus(sourceRoot);
    }

    async function createLane() {
      const payload = {
        target: $('target').value,
        source: sourceRoot
      };
      if (!payload.source) {
        setStatus('Open a root folder first');
        return;
      }
      if (!payload.target) {
        setStatus('Enter a world name');
        return;
      }
      setStatus(`Creating ${payload.target}`);
      try {
        log(await api('/api/new', body(payload)));
        await refresh();
      } catch (error) {
        setStatus(error.message);
        log(error.message);
      }
    }

    async function openApp(env_id, app) {
      try {
        log(await api('/api/open', body({
          env_id,
          app,
          relative_path: ''
        })));
      } catch (error) {
        setStatus(error.message);
        log(error.message);
      }
    }

    async function changed(env_id) {
      try {
        const data = await api('/api/export', body({env_id}));
        log(data.text || data.stdout || data);
      } catch (error) {
        setStatus(error.message);
        log(error.message);
      }
    }

    async function removeLane(env_id) {
      if (!confirm(`Remove ${env_id}?`)) return;
      try {
        log(await api('/api/remove', body({env_id})));
        await refresh();
      } catch (error) {
        setStatus(error.message);
        log(error.message);
      }
    }

    function clearOutput() {
      $('output').textContent = '';
    }

    function escapeHtml(value) {
      return String(value ?? '').replace(/[&<>"']/g, c => ({
        '&': '&amp;',
        '<': '&lt;',
        '>': '&gt;',
        '"': '&quot;',
        "'": '&#39;'
      })[c]);
    }

    function escapeAttr(value) {
      return escapeHtml(value).replaceAll('\\n', '');
    }

    function suggestWorldName(root) {
      const parts = String(root || '').split(/[\\/]+/).filter(Boolean);
      const leaf = (parts[parts.length - 1] || 'world')
        .replace(/[^A-Za-z0-9_.-]+/g, '-')
        .replace(/^-+|-+$/g, '');
      return leaf ? `${leaf}-1` : 'world-1';
    }

    api('/api/config').then(data => {
      $('meta').textContent = `${data.platform} / ${data.agentfs}`;
    }).catch(error => {
      $('meta').textContent = error.message;
    });
    refresh();
  </script>
</body>
</html>
"#;
