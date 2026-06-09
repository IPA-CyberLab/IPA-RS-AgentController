use crate::command::{CmdOutput, CommandRunner};
use crate::model::{unit_name, Env, EnvState};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Nspawn {
    runner: CommandRunner,
    config_dir: PathBuf,
    network_dir: PathBuf,
}

impl Default for Nspawn {
    fn default() -> Self {
        Self {
            runner: CommandRunner,
            config_dir: PathBuf::from("/etc/systemd/nspawn"),
            network_dir: PathBuf::from("/etc/systemd/network"),
        }
    }
}

impl Nspawn {
    const PRIVATE_NAT_ZONE: &str = "agent-forkd";
    const PRIVATE_NAT_BRIDGE: &str = "vz-agent-forkd";
    const INACCESSIBLE_PATHS: &[&str] = &[
        "/agentfs",
        "/run/agent-forkd.sock",
        "/run/docker.sock",
        "/var/run/docker.sock",
    ];

    pub fn config_text(env: &Env) -> String {
        let network = if env.limits.network == "private-nat" {
            format!("VirtualEthernet=yes\nZone={}\n", Self::PRIVATE_NAT_ZONE)
        } else {
            "Private=yes\n".to_string()
        };
        format!(
            "[Exec]\nBoot=yes\nPrivateUsers=yes\nHostname={machine}\n\n[Files]\nReadOnly=no\n\n[Network]\n{network}",
            machine = env.machine_name,
            network = network
        )
    }

    pub fn private_nat_network_text() -> String {
        format!(
            "[Match]\nName={bridge}\n\n[Network]\nAddress=10.77.0.1/24\nDHCPServer=yes\nIPMasquerade=ipv4\nIPForward=ipv4\n\n[DHCPServer]\nPoolOffset=100\nPoolSize=100\nEmitDNS=yes\nDNS=1.1.1.1\n",
            bridge = Self::PRIVATE_NAT_BRIDGE
        )
    }

    pub fn config_path(&self, env: &Env) -> PathBuf {
        self.config_dir.join(format!("{}.nspawn", env.machine_name))
    }

    pub async fn write_config(&self, env: &Env) -> Result<PathBuf> {
        let path = self.config_path(env);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, Self::config_text(env)).await?;
        Ok(path)
    }

    pub async fn remove_config(&self, env: &Env) -> Result<()> {
        let path = self.config_path(env);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn write_private_nat_network_config(&self) -> Result<PathBuf> {
        let path = self.network_dir.join("80-agent-forkd-private-nat.network");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, Self::private_nat_network_text()).await?;
        Ok(path)
    }

    pub fn start_args(env: &Env, log_path: Option<&Path>) -> Result<Vec<String>> {
        env.limits.validate()?;
        let unit = unit_name(&env.id);
        let mut args = vec![
            format!("--unit={unit}"),
            "--collect".to_string(),
            "--property=Delegate=yes".to_string(),
        ];
        args.extend(systemd_limit_properties(env));
        if let Some(path) = log_path {
            args.push(format!(
                "--property=StandardOutput=append:{}",
                path.display()
            ));
            args.push(format!(
                "--property=StandardError=append:{}",
                path.display()
            ));
        }
        args.extend([
            "systemd-nspawn".to_string(),
            "--machine".to_string(),
            env.machine_name.clone(),
            "--directory".to_string(),
            env.rootfs_path.display().to_string(),
            "--boot".to_string(),
            "--private-users=yes".to_string(),
            "--register=yes".to_string(),
        ]);
        args.extend(
            Self::INACCESSIBLE_PATHS
                .iter()
                .map(|path| format!("--inaccessible={path}")),
        );
        if env.limits.network == "private-nat" {
            args.push("--network-veth".to_string());
            args.push(format!("--network-zone={}", Self::PRIVATE_NAT_ZONE));
        } else {
            args.push("--private-network".to_string());
        }
        Ok(args)
    }

    pub async fn start(&self, env: &Env, log_path: Option<&Path>) -> Result<()> {
        if env.limits.network == "private-nat" {
            self.write_private_nat_network_config().await?;
            self.runner
                .run_checked("systemctl", ["reload", "systemd-networkd"])
                .await?;
        }
        self.write_config(env).await?;
        self.runner
            .run_checked("systemd-run", Self::start_args(env, log_path)?)
            .await?;
        Ok(())
    }

    pub async fn stop(&self, machine_name: &str) -> Result<()> {
        let output = self
            .runner
            .run("machinectl", ["terminate", machine_name])
            .await?;
        if output.status == 0 || machinectl_reports_missing_machine(&output.stderr) {
            Ok(())
        } else {
            Err(anyhow!(
                "machinectl terminate {machine_name} exited with {}: {}{}",
                output.status,
                output.stdout,
                output.stderr
            ))
        }
    }

    pub async fn shell(&self, machine_name: &str) -> Result<()> {
        self.runner
            .run_checked("machinectl", ["shell", machine_name, "/bin/bash"])
            .await?;
        Ok(())
    }

    pub async fn exec(&self, env: &Env, command: &[String], log_path: &Path) -> Result<CmdOutput> {
        let mut args = vec![
            "shell".to_string(),
            env.machine_name.clone(),
            "/bin/bash".to_string(),
            "-lc".to_string(),
        ];
        args.push(shell_join(command));
        let output = self.runner.run("machinectl", args).await?;
        let log = format!(
            "$ {}\n[exit:{}]\n--- stdout ---\n{}--- stderr ---\n{}\n",
            shell_join(command),
            output.status,
            output.stdout,
            output.stderr
        );
        CommandRunner::append_to_file(log_path, &log).await?;
        Ok(output)
    }

    pub async fn refresh_state(&self, env: &mut Env) -> Result<()> {
        let output = self
            .runner
            .run(
                "machinectl",
                ["show", &env.machine_name, "-p", "State", "--value"],
            )
            .await?;
        env.state = machinectl_show_state_result(
            output.status,
            &output.stdout,
            &output.stderr,
            &env.machine_name,
        )?;
        Ok(())
    }
}

fn systemd_limit_properties(env: &Env) -> Vec<String> {
    let mut properties = Vec::new();
    if !is_unlimited_str(&env.limits.cpu_max) {
        properties.push(format!("--property=CPUQuota={}", env.limits.cpu_max));
    }
    if !is_unlimited_str(&env.limits.memory_max) {
        properties.push(format!("--property=MemoryMax={}", env.limits.memory_max));
    }
    if env.limits.pids_max != 0 {
        properties.push(format!("--property=TasksMax={}", env.limits.pids_max));
    }
    if !is_unlimited_str(&env.limits.max_runtime) {
        properties.push(format!(
            "--property=RuntimeMaxSec={}",
            env.limits.max_runtime
        ));
    }
    properties
}

fn is_unlimited_str(value: &str) -> bool {
    let value = value.trim();
    value == "0"
        || value.eq_ignore_ascii_case("unlimited")
        || value.eq_ignore_ascii_case("infinity")
}

fn machinectl_reports_missing_machine(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no machine") || stderr.contains("not exist") || stderr.contains("not found")
}

fn machinectl_show_state_result(
    status: i32,
    stdout: &str,
    stderr: &str,
    machine_name: &str,
) -> Result<EnvState> {
    if status == 0 {
        return Ok(if stdout.trim() == "running" {
            EnvState::Running
        } else {
            EnvState::Stopped
        });
    }
    if machinectl_reports_missing_machine(stderr) {
        return Ok(EnvState::Stopped);
    }
    Err(anyhow!(
        "machinectl show {machine_name} exited with {status}: {stdout}{stderr}"
    ))
}

fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_./:=+".contains(&b))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{machine_name, Limits};
    use chrono::Utc;

    #[test]
    fn nspawn_config_keeps_private_users_and_network() {
        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        let text = Nspawn::config_text(&env);
        assert!(text.contains("PrivateUsers=yes"));
        assert!(text.contains("VirtualEthernet=yes"));
        assert!(text.contains("Zone=agent-forkd"));
        assert!(text.contains("Hostname=af-codex-1"));
    }

    #[test]
    fn start_args_apply_limits_and_namespaces() {
        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        let args = Nspawn::start_args(
            &env,
            Some(Path::new("/agentfs/envs/codex-1/logs/nspawn.log")),
        )
        .unwrap();
        assert!(args.contains(&"--private-users=yes".to_string()));
        assert!(args.contains(&"--network-veth".to_string()));
        assert!(args.contains(&"--network-zone=agent-forkd".to_string()));
        assert!(args.contains(&"--inaccessible=/agentfs".to_string()));
        assert!(args.contains(&"--inaccessible=/run/docker.sock".to_string()));
        assert!(args.contains(&"--inaccessible=/var/run/docker.sock".to_string()));
        assert!(args.contains(&"--property=CPUQuota=400%".to_string()));
        assert!(args.contains(&"--property=MemoryMax=16G".to_string()));
        assert!(args.contains(&"--property=TasksMax=4096".to_string()));
        assert!(args.contains(
            &"--property=StandardOutput=append:/agentfs/envs/codex-1/logs/nspawn.log".to_string()
        ));
    }

    #[test]
    fn private_network_profile_has_no_egress_veth() {
        let mut env = Env {
            id: "locked-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/locked-1/rootfs".into(),
            machine_name: machine_name("locked-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        env.limits.network = "private".to_string();
        let args = Nspawn::start_args(&env, None).unwrap();
        assert!(args.contains(&"--private-network".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--network-zone")));
    }

    #[test]
    fn private_nat_networkd_config_enables_masquerade() {
        let text = Nspawn::private_nat_network_text();
        assert!(text.contains("Name=vz-agent-forkd"));
        assert!(text.contains("DHCPServer=yes"));
        assert!(text.contains("IPMasquerade=ipv4"));
        assert!(text.contains("IPForward=ipv4"));
    }

    #[test]
    fn missing_machine_stop_errors_are_idempotent() {
        assert!(machinectl_reports_missing_machine(
            "No machine 'af-codex-1' known"
        ));
        assert!(machinectl_reports_missing_machine(
            "Machine af-codex-1 does not exist"
        ));
        assert!(!machinectl_reports_missing_machine("Access denied"));
    }

    #[test]
    fn machinectl_show_state_maps_missing_machine_to_stopped() {
        assert_eq!(
            machinectl_show_state_result(0, "running\n", "", "af-codex-1").unwrap(),
            EnvState::Running
        );
        assert_eq!(
            machinectl_show_state_result(0, "closing\n", "", "af-codex-1").unwrap(),
            EnvState::Stopped
        );
        assert_eq!(
            machinectl_show_state_result(1, "", "No machine 'af-codex-1' known", "af-codex-1")
                .unwrap(),
            EnvState::Stopped
        );
    }

    #[test]
    fn machinectl_show_state_reports_unexpected_failures() {
        let error = machinectl_show_state_result(1, "", "Access denied", "af-codex-1")
            .unwrap_err()
            .to_string();

        assert!(error.contains("machinectl show af-codex-1 exited with 1"));
        assert!(error.contains("Access denied"));
    }

    #[tokio::test]
    async fn remove_config_deletes_generated_nspawn_file() {
        let dir = tempfile::tempdir().unwrap();
        let nspawn = Nspawn {
            runner: CommandRunner,
            config_dir: dir.path().join("nspawn"),
            network_dir: dir.path().join("network"),
        };
        let env = Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/codex-1/rootfs".into(),
            machine_name: machine_name("codex-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };

        let path = nspawn.write_config(&env).await.unwrap();
        assert!(path.exists());
        nspawn.remove_config(&env).await.unwrap();
        assert!(!path.exists());
        nspawn.remove_config(&env).await.unwrap();
    }

    #[test]
    fn zero_limits_are_omitted_as_unlimited() {
        let mut env = Env {
            id: "unlimited-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/unlimited-1/rootfs".into(),
            machine_name: machine_name("unlimited-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        env.limits.cpu_max = "0".to_string();
        env.limits.memory_max = "0".to_string();
        env.limits.pids_max = 0;
        env.limits.max_runtime = "0".to_string();
        let args = Nspawn::start_args(&env, None).unwrap();
        assert!(!args.iter().any(|arg| arg.contains("CPUQuota")));
        assert!(!args.iter().any(|arg| arg.contains("MemoryMax")));
        assert!(!args.iter().any(|arg| arg.contains("TasksMax")));
        assert!(!args.iter().any(|arg| arg.contains("RuntimeMaxSec")));
    }

    #[test]
    fn nonzero_max_runtime_sets_systemd_runtime_limit() {
        let mut env = Env {
            id: "timed-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/timed-1/rootfs".into(),
            machine_name: machine_name("timed-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        env.limits.max_runtime = "6h".to_string();
        let args = Nspawn::start_args(&env, None).unwrap();
        assert!(args.contains(&"--property=RuntimeMaxSec=6h".to_string()));
    }

    #[test]
    fn start_args_reject_unknown_network_modes() {
        let mut env = Env {
            id: "bad-network-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path: "/agentfs/envs/bad-network-1/rootfs".into(),
            machine_name: machine_name("bad-network-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            limits: Limits::default(),
            sessions: Vec::new(),
        };
        env.limits.network = "bridge".to_string();

        assert!(Nspawn::start_args(&env, None)
            .unwrap_err()
            .to_string()
            .contains("unsupported network mode"));
    }

    #[test]
    fn shell_join_quotes_spaces() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "whoami && pwd".into()]),
            "bash -lc 'whoami && pwd'"
        );
    }
}
