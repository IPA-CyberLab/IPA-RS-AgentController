use agent_core::config::AgentConfig;
use agent_core::model::LimitOverrides;
use agent_core::protocol::{Request, Response};
use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::Command as StdCommand;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Parser)]
#[command(name = "agentctl", about = "Control forked dev environments")]
struct Cli {
    #[arg(long, env = "AGENTFS", default_value = "/agentfs", global = true)]
    agentfs: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Base {
        #[command(subcommand)]
        command: BaseCommand,
    },
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    Shell {
        env_id: String,
    },
    Exec(ExecArgs),
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Diff {
        env_id: String,
    },
    Export(ExportArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    agentfs: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum BaseCommand {
    Freeze {
        #[arg(long)]
        name: String,
        #[arg(long)]
        from: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum EnvCommand {
    Create {
        env_id: String,
        #[arg(long = "from")]
        base: String,
        #[arg(long, default_value = "privileged-dev")]
        profile: String,
        #[arg(long)]
        cpu_max: Option<String>,
        #[arg(long)]
        memory_max: Option<String>,
        #[arg(long)]
        pids_max: Option<u32>,
        #[arg(long)]
        disk_max: Option<String>,
        #[arg(long)]
        network: Option<String>,
        #[arg(long)]
        idle_timeout: Option<String>,
        #[arg(long)]
        max_runtime: Option<String>,
    },
    Start {
        env_id: String,
    },
    Stop {
        env_id: String,
    },
    Destroy {
        env_id: String,
    },
    List,
    Status {
        env_id: String,
    },
}

#[derive(Debug, Args)]
struct ExecArgs {
    env_id: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Create(SessionCreateArgs),
    Attach { env_id: String, session_id: String },
    List { env_id: String },
    Logs { env_id: String, session_id: String },
}

#[derive(Debug, Args)]
struct SessionCreateArgs {
    env_id: String,
    session_id: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct ExportArgs {
    env_id: String,
    #[arg(long = "type")]
    export_type: ExportKind,
}

#[derive(Debug, Clone, ValueEnum)]
enum ExportKind {
    WorkspacePatch,
    RootfsChangedPaths,
    DpkgDelta,
}

impl ExportKind {
    fn as_wire(&self) -> &'static str {
        match self {
            Self::WorkspacePatch => "workspace-patch",
            Self::RootfsChangedPaths => "rootfs-changed-paths",
            Self::DpkgDelta => "dpkg-delta",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let request = to_request(&cli);
    let config = AgentConfig::new(cli.agentfs.clone());
    let response = call(&config.socket_path, request).await?;
    print_response(response)
}

fn to_request(cli: &Cli) -> Request {
    match &cli.command {
        Command::Init(args) => Request::Init {
            agentfs: args.agentfs.clone().unwrap_or_else(|| cli.agentfs.clone()),
        },
        Command::Base { command } => match command {
            BaseCommand::Freeze { name, from } => Request::BaseFreeze {
                name: name.clone(),
                from: from.clone(),
            },
        },
        Command::Env { command } => match command {
            EnvCommand::Create {
                env_id,
                base,
                profile,
                cpu_max,
                memory_max,
                pids_max,
                disk_max,
                network,
                idle_timeout,
                max_runtime,
            } => Request::EnvCreate {
                id: env_id.clone(),
                base: base.clone(),
                profile: profile.clone(),
                limits: LimitOverrides {
                    cpu_max: cpu_max.clone(),
                    memory_max: memory_max.clone(),
                    pids_max: *pids_max,
                    disk_max: disk_max.clone(),
                    network: network.clone(),
                    idle_timeout: idle_timeout.clone(),
                    max_runtime: max_runtime.clone(),
                },
            },
            EnvCommand::Start { env_id } => Request::EnvStart { id: env_id.clone() },
            EnvCommand::Stop { env_id } => Request::EnvStop { id: env_id.clone() },
            EnvCommand::Destroy { env_id } => Request::EnvDestroy { id: env_id.clone() },
            EnvCommand::List => Request::EnvList,
            EnvCommand::Status { env_id } => Request::EnvStatus { id: env_id.clone() },
        },
        Command::Shell { env_id } => Request::Shell { id: env_id.clone() },
        Command::Exec(args) => Request::Exec {
            id: args.env_id.clone(),
            command: args.command.clone(),
        },
        Command::Session { command } => match command {
            SessionCommand::Create(args) => Request::SessionCreate {
                env_id: args.env_id.clone(),
                session_id: args.session_id.clone(),
                command: args.command.clone(),
            },
            SessionCommand::Attach { env_id, session_id } => Request::SessionAttach {
                env_id: env_id.clone(),
                session_id: session_id.clone(),
            },
            SessionCommand::List { env_id } => Request::SessionList {
                env_id: env_id.clone(),
            },
            SessionCommand::Logs { env_id, session_id } => Request::SessionLogs {
                env_id: env_id.clone(),
                session_id: session_id.clone(),
            },
        },
        Command::Diff { env_id } => Request::Diff {
            env_id: env_id.clone(),
        },
        Command::Export(args) => Request::Export {
            env_id: args.env_id.clone(),
            export_type: args.export_type.as_wire().to_string(),
        },
    }
}

async fn call(socket_path: &PathBuf, request: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(|error| anyhow!("failed to connect {}: {error}", socket_path.display()))?;
    let bytes = serde_json::to_vec(&request)?;
    stream.write_all(&bytes).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        return Err(anyhow!("agent-forkd closed the socket without a response"));
    }
    Ok(serde_json::from_str(&line)?)
}

fn print_response(response: Response) -> Result<()> {
    match response {
        Response::Ok => Ok(()),
        Response::Text { text } => {
            print!("{text}");
            Ok(())
        }
        Response::Exec {
            status,
            stdout,
            stderr,
        } => {
            print!("{stdout}");
            eprint!("{stderr}");
            std::process::exit(status);
        }
        Response::Envs { envs } => {
            println!("ENV\tBASE\tSTATE\tDISK_USED\tSESSIONS");
            for status in envs {
                let env = status.env;
                let sessions = if env.sessions.is_empty() {
                    "-".to_string()
                } else {
                    env.sessions.join(",")
                };
                println!(
                    "{}\t{}\t{:?}\t{}\t{}",
                    env.id,
                    env.base_id,
                    env.state,
                    status.disk_used.unwrap_or_else(|| "-".to_string()),
                    sessions
                );
            }
            Ok(())
        }
        Response::EnvStatus { status } => {
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Response::Sessions { sessions } => {
            println!("SESSION\tENV\tSTATE\tCOMMAND\tLOG");
            for session in sessions {
                println!(
                    "{}\t{}\t{:?}\t{}\t{}",
                    session.id,
                    session.env_id,
                    session.state,
                    session.command,
                    session.log_path.display()
                );
            }
            Ok(())
        }
        Response::Attach {
            machine_name,
            session_id,
        } => {
            let command = format!(
                "tmux attach-session -t '{}'",
                session_id.replace('\'', "'\\''")
            );
            let status = StdCommand::new("machinectl")
                .args(["shell", &machine_name, "/bin/bash", "-lc", &command])
                .status()?;
            std::process::exit(status.code().unwrap_or(128));
        }
        Response::Error { message } => Err(anyhow!(message)),
    }
}
