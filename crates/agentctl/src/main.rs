use agent_core::config::AgentConfig;
use agent_core::model::{EnvState, LimitOverrides, SessionState};
use agent_core::protocol::{parse_response_json, Request, Response};
use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Parser)]
#[command(name = "agentctl", about = "Control forked dev environments")]
struct Cli {
    #[arg(long, env = "AGENTFS", default_value = "/agentfs", global = true)]
    agentfs: PathBuf,
    #[arg(long, env = "AGENT_FORKD_CONFIG", global = true)]
    config: Option<PathBuf>,
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
        #[arg(long)]
        profile: Option<String>,
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
    Detach { env_id: String, session_id: String },
    Kill { env_id: String, session_id: String },
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
    let config =
        AgentConfig::load_or_default(cli.config.as_deref(), effective_agentfs(&cli)).await?;
    let request = to_request(&cli, &config)?;
    let response = call(&config.socket_path, request).await?;
    print_response(response)
}

fn effective_agentfs(cli: &Cli) -> PathBuf {
    match &cli.command {
        Command::Init(args) => args.agentfs.clone().unwrap_or_else(|| cli.agentfs.clone()),
        _ => cli.agentfs.clone(),
    }
}

fn to_request(cli: &Cli, config: &AgentConfig) -> Result<Request> {
    match &cli.command {
        Command::Init(args) => {
            let agentfs = args.agentfs.clone().unwrap_or_else(|| cli.agentfs.clone());
            if config.agentfs != agentfs {
                return Err(anyhow!(
                    "init --agentfs {} does not match daemon config agentfs {}; use matching --config or AGENTFS",
                    agentfs.display(),
                    config.agentfs.display()
                ));
            }
            Ok(Request::Init { agentfs })
        }
        Command::Base { command } => Ok(match command {
            BaseCommand::Freeze { name, from } => Request::BaseFreeze {
                name: name.clone(),
                from: from.clone(),
            },
        }),
        Command::Env { command } => Ok(match command {
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
                profile: profile
                    .clone()
                    .unwrap_or_else(|| config.default_profile.clone()),
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
        }),
        Command::Shell { env_id } => Ok(Request::Shell { id: env_id.clone() }),
        Command::Exec(args) => Ok(Request::Exec {
            id: args.env_id.clone(),
            command: args.command.clone(),
        }),
        Command::Session { command } => Ok(match command {
            SessionCommand::Create(args) => Request::SessionCreate {
                env_id: args.env_id.clone(),
                session_id: args.session_id.clone(),
                command: args.command.clone(),
            },
            SessionCommand::Attach { env_id, session_id } => Request::SessionAttach {
                env_id: env_id.clone(),
                session_id: session_id.clone(),
            },
            SessionCommand::Detach { env_id, session_id } => Request::SessionDetach {
                env_id: env_id.clone(),
                session_id: session_id.clone(),
            },
            SessionCommand::Kill { env_id, session_id } => Request::SessionKill {
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
        }),
        Command::Diff { env_id } => Ok(Request::Diff {
            env_id: env_id.clone(),
        }),
        Command::Export(args) => Ok(Request::Export {
            env_id: args.env_id.clone(),
            export_type: args.export_type.as_wire().to_string(),
        }),
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
    parse_response_line(socket_path, &line)
}

fn parse_response_line(socket_path: &Path, line: &str) -> Result<Response> {
    parse_response_json(line).map_err(|error| {
        anyhow!(
            "invalid response json from {}: {error}: {}",
            socket_path.display(),
            line.trim_end()
        )
    })
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
                    "{}\t{}\t{}\t{}\t{}",
                    env.id,
                    env.base_id,
                    env_state_label(&env.state),
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
                    "{}\t{}\t{}\t{}\t{}",
                    session.id,
                    session.env_id,
                    session_state_label(&session.state),
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
            let status = StdCommand::new("machinectl")
                .args(machinectl_attach_args(&machine_name, &session_id))
                .status()?;
            std::process::exit(status.code().unwrap_or(128));
        }
        Response::Error { message } => Err(anyhow!(message)),
    }
}

fn machinectl_attach_args(machine_name: &str, session_id: &str) -> Vec<String> {
    vec![
        "shell".to_string(),
        machine_name.to_string(),
        "/bin/bash".to_string(),
        "-lc".to_string(),
        tmux_attach_command(session_id),
    ]
}

fn tmux_attach_command(session_id: &str) -> String {
    format!("tmux attach-session -t {}", shell_quote(session_id))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn env_state_label(state: &EnvState) -> &'static str {
    match state {
        EnvState::Created => "created",
        EnvState::Running => "running",
        EnvState::Stopped => "stopped",
        EnvState::Failed => "failed",
        EnvState::QuotaExceeded => "quota_exceeded",
    }
}

fn session_state_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Running => "running",
        SessionState::Stopped => "stopped",
        SessionState::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        effective_agentfs, env_state_label, machinectl_attach_args, parse_response_line,
        session_state_label, shell_quote, tmux_attach_command, to_request, Cli, Command,
        EnvCommand, ExportArgs, ExportKind, InitArgs,
    };
    use agent_core::config::{AgentConfig, Profile};
    use agent_core::model::{EnvState, SessionState};
    use agent_core::protocol::Request;
    use std::path::PathBuf;

    #[test]
    fn init_agentfs_override_controls_request_and_socket_base() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Init(InitArgs {
                agentfs: Some(PathBuf::from("/custom-agentfs")),
            }),
        };

        assert_eq!(effective_agentfs(&cli), PathBuf::from("/custom-agentfs"));
        match to_request(&cli, &AgentConfig::new(PathBuf::from("/custom-agentfs"))).unwrap() {
            Request::Init { agentfs } => assert_eq!(agentfs, PathBuf::from("/custom-agentfs")),
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn init_rejects_agentfs_mismatch_with_loaded_config() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: Some(PathBuf::from("/etc/agent-forkd/config.json")),
            command: Command::Init(InitArgs {
                agentfs: Some(PathBuf::from("/custom-agentfs")),
            }),
        };
        let config = AgentConfig::new(PathBuf::from("/agentfs"));

        let error = to_request(&cli, &config).unwrap_err().to_string();

        assert!(error.contains("does not match daemon config agentfs"));
        assert!(error.contains("/custom-agentfs"));
        assert!(error.contains("/agentfs"));
    }

    #[test]
    fn env_create_uses_configured_default_profile() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Env {
                command: EnvCommand::Create {
                    env_id: "codex-1".to_string(),
                    base: "base-001".to_string(),
                    profile: None,
                    cpu_max: None,
                    memory_max: None,
                    pids_max: None,
                    disk_max: None,
                    network: None,
                    idle_timeout: None,
                    max_runtime: None,
                },
            },
        };
        let mut config = AgentConfig::new(PathBuf::from("/agentfs"));
        config.default_profile = "custom-dev".to_string();
        config.profiles.push(Profile {
            name: "custom-dev".to_string(),
            limits: Default::default(),
        });

        match to_request(&cli, &config).unwrap() {
            Request::EnvCreate { profile, .. } => assert_eq!(profile, "custom-dev"),
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn env_create_profile_flag_overrides_configured_default() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Env {
                command: EnvCommand::Create {
                    env_id: "codex-1".to_string(),
                    base: "base-001".to_string(),
                    profile: Some("explicit-dev".to_string()),
                    cpu_max: None,
                    memory_max: None,
                    pids_max: None,
                    disk_max: None,
                    network: None,
                    idle_timeout: None,
                    max_runtime: None,
                },
            },
        };
        let config = AgentConfig::new(PathBuf::from("/agentfs"));

        match to_request(&cli, &config).unwrap() {
            Request::EnvCreate { profile, .. } => assert_eq!(profile, "explicit-dev"),
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn env_create_forwards_all_resource_overrides() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Env {
                command: EnvCommand::Create {
                    env_id: "codex-1".to_string(),
                    base: "base-001".to_string(),
                    profile: Some("privileged-dev".to_string()),
                    cpu_max: Some("800%".to_string()),
                    memory_max: Some("32G".to_string()),
                    pids_max: Some(8192),
                    disk_max: Some("200G".to_string()),
                    network: Some("private".to_string()),
                    idle_timeout: Some("30m".to_string()),
                    max_runtime: Some("6h".to_string()),
                },
            },
        };

        match to_request(&cli, &AgentConfig::new(PathBuf::from("/agentfs"))).unwrap() {
            Request::EnvCreate {
                id,
                base,
                profile,
                limits,
            } => {
                assert_eq!(id, "codex-1");
                assert_eq!(base, "base-001");
                assert_eq!(profile, "privileged-dev");
                assert_eq!(limits.cpu_max.as_deref(), Some("800%"));
                assert_eq!(limits.memory_max.as_deref(), Some("32G"));
                assert_eq!(limits.pids_max, Some(8192));
                assert_eq!(limits.disk_max.as_deref(), Some("200G"));
                assert_eq!(limits.network.as_deref(), Some("private"));
                assert_eq!(limits.idle_timeout.as_deref(), Some("30m"));
                assert_eq!(limits.max_runtime.as_deref(), Some("6h"));
            }
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn shell_command_maps_to_shell_request() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Shell {
                env_id: "codex-1".to_string(),
            },
        };

        match to_request(&cli, &AgentConfig::new(PathBuf::from("/agentfs"))).unwrap() {
            Request::Shell { id } => assert_eq!(id, "codex-1"),
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn diff_command_maps_to_diff_request() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Diff {
                env_id: "codex-1".to_string(),
            },
        };

        match to_request(&cli, &AgentConfig::new(PathBuf::from("/agentfs"))).unwrap() {
            Request::Diff { env_id } => assert_eq!(env_id, "codex-1"),
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn export_command_uses_wire_export_type() {
        let cli = Cli {
            agentfs: PathBuf::from("/agentfs"),
            config: None,
            command: Command::Export(ExportArgs {
                env_id: "codex-1".to_string(),
                export_type: ExportKind::WorkspacePatch,
            }),
        };

        match to_request(&cli, &AgentConfig::new(PathBuf::from("/agentfs"))).unwrap() {
            Request::Export {
                env_id,
                export_type,
            } => {
                assert_eq!(env_id, "codex-1");
                assert_eq!(export_type, "workspace-patch");
            }
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn response_parse_errors_include_socket_and_payload() {
        let error = parse_response_line(&PathBuf::from("/agentfs/runtime/sockets/a.sock"), "{bad")
            .unwrap_err()
            .to_string();

        assert!(error.contains("/agentfs/runtime/sockets/a.sock"));
        assert!(error.contains("{bad"));
    }

    #[test]
    fn response_parse_rejects_unknown_fields() {
        let error = parse_response_line(
            &PathBuf::from("/agentfs/runtime/sockets/a.sock"),
            r#"{"type":"ok","unexpected":"field"}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("invalid response json"));
        assert!(error.contains("unexpected"));
    }

    #[test]
    fn attach_args_enter_child_and_attach_tmux_session() {
        assert_eq!(
            machinectl_attach_args("af-codex-1", "dev"),
            vec![
                "shell".to_string(),
                "af-codex-1".to_string(),
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "tmux attach-session -t 'dev'".to_string(),
            ]
        );
    }

    #[test]
    fn attach_command_quotes_session_for_child_shell() {
        assert_eq!(shell_quote("shell's dev"), "'shell'\\''s dev'");
        assert_eq!(
            tmux_attach_command("shell's dev"),
            "tmux attach-session -t 'shell'\\''s dev'"
        );
    }

    #[test]
    fn table_state_labels_match_wire_names() {
        assert_eq!(env_state_label(&EnvState::Running), "running");
        assert_eq!(env_state_label(&EnvState::QuotaExceeded), "quota_exceeded");
        assert_eq!(session_state_label(&SessionState::Running), "running");
        assert_eq!(session_state_label(&SessionState::Stopped), "stopped");
    }
}
