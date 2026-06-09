use agent_core::config::AgentConfig;
use agent_core::protocol::{Request, Response};
use agent_core::AgentService;
use anyhow::{anyhow, Result};
use clap::Parser;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(name = "agent-forkd", about = "Forked dev environment daemon")]
struct Args {
    #[arg(long, env = "AGENTFS", default_value = "/agentfs")]
    agentfs: PathBuf,
    #[arg(long, env = "AGENT_FORKD_CONFIG")]
    config: Option<PathBuf>,
}

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

async fn bind_control_socket(path: &Path) -> Result<UnixListener> {
    prepare_socket_path(path).await?;
    let listener = UnixListener::bind(path)?;
    tokio::fs::set_permissions(path, Permissions::from_mode(0o666)).await?;
    Ok(listener)
}

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

fn parse_request_line(line: &str) -> std::result::Result<Request, Response> {
    serde_json::from_str(line).map_err(|error| Response::Error {
        message: format!("invalid request json: {error}"),
    })
}

async fn write_response<W>(write: &mut W, response: &Response) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(response)?;
    write.write_all(&bytes).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

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
}
