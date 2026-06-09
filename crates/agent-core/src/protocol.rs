use crate::model::{Env, EnvStatus, LimitOverrides, Session};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Init {
        agentfs: PathBuf,
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
#[serde(tag = "type", rename_all = "snake_case")]
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
        envs: Vec<Env>,
    },
    EnvStatus {
        status: Box<EnvStatus>,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    Error {
        message: String,
    },
}
