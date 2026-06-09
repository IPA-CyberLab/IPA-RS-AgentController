#[cfg(unix)]
use agent_core::config::AgentConfig;
#[cfg(unix)]
use agent_core::protocol::{parse_request_json, Request, Response};
#[cfg(unix)]
use agent_core::AgentService;
#[cfg(unix)]
use anyhow::{anyhow, Result};
#[cfg(unix)]
use clap::Parser;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tracing::{error, info};

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "agent-forkd", about = "Forked dev environment daemon")]
struct Args {
    #[arg(long, env = "AGENTFS", default_value = "/agentfs")]
    agentfs: PathBuf,
    #[arg(long, env = "AGENT_FORKD_CONFIG")]
    config: Option<PathBuf>,
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let config = AgentConfig::load_or_default(args.config.as_deref(), args.agentfs).await?;
    let service = AgentService::new(config.clone());
    let listener = bind_control_socket(&config.socket_path).await?;
    info!("listening on {}", config.socket_path.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(service, stream).await {
                error!("{error:#}");
            }
        });
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("agent-forkd is supported only on Unix-like platforms.");
    std::process::exit(1);
}

#[cfg(unix)]
async fn bind_control_socket(path: &Path) -> Result<UnixListener> {
    prepare_socket_path(path).await?;
    let listener = UnixListener::bind(path)?;
    tokio::fs::set_permissions(path, Permissions::from_mode(0o666)).await?;
    Ok(listener)
}

#[cfg(unix)]
async fn handle_client(service: AgentService, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let request = match parse_request_line(&line) {
            Ok(request) => request,
            Err(response) => {
                write_response(&mut write, &response).await?;
                break;
            }
        };
        let response = service.handle(request).await;
        write_response(&mut write, &response).await?;
        if matches!(response, Response::Error { .. }) {
            break;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn parse_request_line(line: &str) -> std::result::Result<Request, Response> {
    parse_request_json(line).map_err(|error| Response::Error {
        message: format!("invalid request json: {error}"),
    })
}

#[cfg(unix)]
async fn write_response<W>(write: &mut W, response: &Response) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(response)?;
    write.write_all(&bytes).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

#[cfg(unix)]
async fn prepare_socket_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent", path.display()))?;
    tokio::fs::create_dir_all(parent).await?;
    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).await.is_ok() {
        return Err(anyhow!(
            "{} is already accepting connections; agent-forkd may already be running",
            path.display()
        ));
    }
    tokio::fs::remove_file(path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bind_control_socket, parse_request_line, prepare_socket_path};
    use agent_core::protocol::Response;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn prepare_socket_path_removes_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent-forkd.sock");
        tokio::fs::write(&socket, b"stale").await.unwrap();

        prepare_socket_path(&socket).await.unwrap();

        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn prepare_socket_path_rejects_active_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent-forkd.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let error = prepare_socket_path(&socket).await.unwrap_err();

        assert!(error.to_string().contains("already accepting connections"));
        assert!(socket.exists());
    }

    #[tokio::test]
    async fn bind_control_socket_allows_local_agentctl_clients() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent-forkd.sock");
        let _listener = bind_control_socket(&socket).await.unwrap();

        let mode = tokio::fs::metadata(&socket)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o666);
    }

    #[test]
    fn invalid_request_line_returns_error_response() {
        let response = parse_request_line("{not json").unwrap_err();

        match response {
            Response::Error { message } => {
                assert!(message.contains("invalid request json"));
            }
            other => panic!("unexpected response {other:?}"),
        }
    }

    #[test]
    fn request_line_rejects_unknown_fields() {
        let response = parse_request_line(r#"{"type":"ping","unexpected":"field"}"#).unwrap_err();

        match response {
            Response::Error { message } => {
                assert!(message.contains("invalid request json"));
                assert!(message.contains("unexpected"));
            }
            other => panic!("unexpected response {other:?}"),
        }
    }

    #[test]
    fn packaged_systemd_unit_starts_daemon_with_config() {
        let unit = include_str!("../../../packaging/systemd/agent-forkd.service");

        assert!(unit.contains("Requires=systemd-networkd.service systemd-machined.service"));
        assert!(unit.contains("Environment=AGENTFS=/agentfs"));
        assert!(unit.contains("Environment=AGENT_FORKD_CONFIG=/etc/agent-forkd/config.json"));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/agent-forkd --agentfs ${AGENTFS} --config ${AGENT_FORKD_CONFIG}"
        ));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }
}
