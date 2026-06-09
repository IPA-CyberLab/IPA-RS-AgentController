use crate::model::Limits;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub agentfs: PathBuf,
    pub socket_path: PathBuf,
    pub default_profile: String,
    pub profiles: Vec<Profile>,
}

impl AgentConfig {
    pub fn new(agentfs: PathBuf) -> Self {
        let socket_path = agentfs.join("runtime/sockets/agent-forkd.sock");
        Self {
            agentfs,
            socket_path,
            default_profile: "privileged-dev".to_string(),
            profiles: vec![Profile::privileged_dev()],
        }
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub limits: Limits,
}

impl Profile {
    pub fn privileged_dev() -> Self {
        Self {
            name: "privileged-dev".to_string(),
            limits: Limits::default(),
        }
    }
}
