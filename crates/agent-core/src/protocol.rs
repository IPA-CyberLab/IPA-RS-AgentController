use crate::model::{EnvStatus, LimitOverrides, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Init {
        agentfs: PathBuf,
    },
    New {
        target: String,
        base: String,
        from: PathBuf,
        profile: String,
        limits: LimitOverrides,
        command: Vec<String>,
    },
    BaseFreeze {
        name: String,
        from: PathBuf,
    },
    EnvCreate {
        id: String,
        base: String,
        profile: String,
        limits: LimitOverrides,
    },
    EnvStart {
        id: String,
    },
    EnvStop {
        id: String,
    },
    EnvDestroy {
        id: String,
    },
    EnvList,
    EnvStatus {
        id: String,
    },
    Exec {
        id: String,
        command: Vec<String>,
    },
    Shell {
        id: String,
    },
    SessionCreate {
        env_id: String,
        session_id: String,
        command: Vec<String>,
    },
    SessionAttach {
        env_id: String,
        session_id: String,
    },
    SessionDetach {
        env_id: String,
        session_id: String,
    },
    SessionKill {
        env_id: String,
        session_id: String,
    },
    SessionList {
        env_id: String,
    },
    SessionLogs {
        env_id: String,
        session_id: String,
    },
    Diff {
        env_id: String,
    },
    Export {
        env_id: String,
        export_type: String,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Ok,
    Text {
        text: String,
    },
    Exec {
        status: i32,
        stdout: String,
        stderr: String,
    },
    Envs {
        envs: Vec<EnvStatus>,
    },
    EnvStatus {
        status: Box<EnvStatus>,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    Attach {
        machine_name: String,
        session_id: String,
    },
    Error {
        message: String,
    },
}

pub fn parse_request_json(line: &str) -> Result<Request, String> {
    let value = parse_protocol_value(line)?;
    reject_unknown_fields(&value, request_allowed_fields)?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub fn parse_response_json(line: &str) -> Result<Response, String> {
    let value = parse_protocol_value(line)?;
    reject_unknown_fields(&value, response_allowed_fields)?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn parse_protocol_value(line: &str) -> Result<Value, String> {
    serde_json::from_str(line).map_err(|error| error.to_string())
}

fn reject_unknown_fields(
    value: &Value,
    allowed_fields: fn(&str) -> Option<&'static [&'static str]>,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "protocol message must be a JSON object".to_string())?;
    let message_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "protocol message missing string field type".to_string())?;
    let allowed = allowed_fields(message_type)
        .ok_or_else(|| format!("unknown protocol message type {message_type}"))?;
    for field in object.keys() {
        if !allowed.iter().any(|allowed| allowed == field) {
            return Err(format!(
                "unknown field {field} for protocol message type {message_type}"
            ));
        }
    }
    Ok(())
}

fn request_allowed_fields(message_type: &str) -> Option<&'static [&'static str]> {
    Some(match message_type {
        "init" => &["type", "agentfs"],
        "new" => &[
            "type", "target", "base", "from", "profile", "limits", "command",
        ],
        "base_freeze" => &["type", "name", "from"],
        "env_create" => &["type", "id", "base", "profile", "limits"],
        "env_start" | "env_stop" | "env_destroy" | "env_status" | "shell" => &["type", "id"],
        "env_list" | "ping" => &["type"],
        "exec" => &["type", "id", "command"],
        "session_create" => &["type", "env_id", "session_id", "command"],
        "session_attach" | "session_detach" | "session_kill" | "session_logs" => {
            &["type", "env_id", "session_id"]
        }
        "session_list" | "diff" => &["type", "env_id"],
        "export" => &["type", "env_id", "export_type"],
        _ => return None,
    })
}

fn response_allowed_fields(message_type: &str) -> Option<&'static [&'static str]> {
    Some(match message_type {
        "ok" => &["type"],
        "text" => &["type", "text"],
        "exec" => &["type", "status", "stdout", "stderr"],
        "envs" => &["type", "envs"],
        "env_status" => &["type", "status"],
        "sessions" => &["type", "sessions"],
        "attach" => &["type", "machine_name", "session_id"],
        "error" => &["type", "message"],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crate::model::{
        machine_name, Env, EnvState, EnvStatus, LimitOverrides, Limits, Session, SessionState,
        SessionType,
    };
    use chrono::Utc;

    use super::{parse_request_json, parse_response_json, Request, Response};

    #[test]
    fn request_parser_rejects_unknown_fields_on_unit_variant() {
        let error = parse_request_json(r#"{"type":"ping","unexpected":"field"}"#).unwrap_err();

        assert!(error.contains("unknown field unexpected"));
    }

    #[test]
    fn response_parser_rejects_unknown_fields_on_unit_variant() {
        let error = parse_response_json(r#"{"type":"ok","unexpected":"field"}"#).unwrap_err();

        assert!(error.contains("unknown field unexpected"));
    }

    #[test]
    fn protocol_parsers_accept_valid_messages() {
        assert!(matches!(
            parse_request_json(r#"{"type":"ping"}"#).unwrap(),
            Request::Ping
        ));
        assert!(matches!(
            parse_response_json(r#"{"type":"ok"}"#).unwrap(),
            Response::Ok
        ));
    }

    #[test]
    fn strict_request_parser_covers_all_variants() {
        for request in sample_requests() {
            let json = serde_json::to_string(&request).unwrap();
            parse_request_json(&json).unwrap_or_else(|error| {
                panic!("strict request parser rejected {json}: {error}");
            });
        }
    }

    #[test]
    fn strict_response_parser_covers_all_variants() {
        for response in sample_responses() {
            let json = serde_json::to_string(&response).unwrap();
            parse_response_json(&json).unwrap_or_else(|error| {
                panic!("strict response parser rejected {json}: {error}");
            });
        }
    }

    fn sample_requests() -> Vec<Request> {
        vec![
            Request::Init {
                agentfs: "/agentfs".into(),
            },
            Request::New {
                target: "codex".to_string(),
                base: "base-001".to_string(),
                from: "/".into(),
                profile: "privileged-dev".to_string(),
                limits: LimitOverrides::default(),
                command: Vec::new(),
            },
            Request::BaseFreeze {
                name: "base-001".to_string(),
                from: "/".into(),
            },
            Request::EnvCreate {
                id: "codex-1".to_string(),
                base: "base-001".to_string(),
                profile: "privileged-dev".to_string(),
                limits: LimitOverrides::default(),
            },
            Request::EnvStart {
                id: "codex-1".to_string(),
            },
            Request::EnvStop {
                id: "codex-1".to_string(),
            },
            Request::EnvDestroy {
                id: "codex-1".to_string(),
            },
            Request::EnvList,
            Request::EnvStatus {
                id: "codex-1".to_string(),
            },
            Request::Exec {
                id: "codex-1".to_string(),
                command: vec!["bash".to_string()],
            },
            Request::Shell {
                id: "codex-1".to_string(),
            },
            Request::SessionCreate {
                env_id: "codex-1".to_string(),
                session_id: "dev".to_string(),
                command: vec!["bash".to_string()],
            },
            Request::SessionAttach {
                env_id: "codex-1".to_string(),
                session_id: "dev".to_string(),
            },
            Request::SessionDetach {
                env_id: "codex-1".to_string(),
                session_id: "dev".to_string(),
            },
            Request::SessionKill {
                env_id: "codex-1".to_string(),
                session_id: "dev".to_string(),
            },
            Request::SessionList {
                env_id: "codex-1".to_string(),
            },
            Request::SessionLogs {
                env_id: "codex-1".to_string(),
                session_id: "dev".to_string(),
            },
            Request::Diff {
                env_id: "codex-1".to_string(),
            },
            Request::Export {
                env_id: "codex-1".to_string(),
                export_type: "dpkg-delta".to_string(),
            },
            Request::Ping,
        ]
    }

    fn sample_responses() -> Vec<Response> {
        let env_status = EnvStatus {
            env: sample_env(),
            disk_used: Some("1G".to_string()),
        };
        vec![
            Response::Ok,
            Response::Text {
                text: "ok".to_string(),
            },
            Response::Exec {
                status: 0,
                stdout: "out".to_string(),
                stderr: String::new(),
            },
            Response::Envs {
                envs: vec![env_status.clone()],
            },
            Response::EnvStatus {
                status: Box::new(env_status),
            },
            Response::Sessions {
                sessions: vec![sample_session()],
            },
            Response::Attach {
                machine_name: "af-codex-1".to_string(),
                session_id: "dev".to_string(),
            },
            Response::Error {
                message: "error".to_string(),
            },
        ]
    }

    fn sample_env() -> Env {
        Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            backend: crate::model::RootfsBackend::Btrfs,
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Running,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            network_policy: Default::default(),
            limits: Limits::default(),
            sessions: vec!["dev".to_string()],
        }
    }

    fn sample_session() -> Session {
        Session {
            id: "dev".to_string(),
            env_id: "codex-1".to_string(),
            command: "bash".to_string(),
            state: SessionState::Running,
            created_at: Utc::now(),
            session_type: SessionType::Pty,
            log_path: "/agentfs/envs/codex-1/logs/sessions/dev.log".into(),
        }
    }
}
