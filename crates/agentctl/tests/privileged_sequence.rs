use std::process::Command;

#[test]
#[ignore = "requires privileged Btrfs/systemd-nspawn Project VM with /agentfs"]
fn goal_sequence_runs_in_privileged_project_vm() {
    let commands: &[&[&str]] = &[
        &["agentctl", "init", "--agentfs", "/agentfs"],
        &[
            "agentctl", "base", "freeze", "--name", "base-001", "--from", "/",
        ],
        &[
            "agentctl",
            "env",
            "create",
            "codex-1",
            "--from",
            "base-001",
            "--profile",
            "privileged-dev",
            "--cpu-max",
            "400%",
            "--memory-max",
            "16G",
            "--pids-max",
            "4096",
            "--disk-max",
            "100G",
        ],
        &[
            "agentctl",
            "env",
            "create",
            "claude-1",
            "--from",
            "base-001",
            "--profile",
            "privileged-dev",
        ],
        &["agentctl", "env", "start", "codex-1"],
        &["agentctl", "env", "start", "claude-1"],
        &["agentctl", "exec", "codex-1", "--", "sudo", "apt", "update"],
        &[
            "agentctl", "exec", "codex-1", "--", "sudo", "apt", "install", "-y", "ripgrep",
        ],
        &["agentctl", "exec", "codex-1", "--", "rg", "--version"],
        &[
            "agentctl",
            "exec",
            "claude-1",
            "--",
            "bash",
            "-lc",
            "command -v rg || true",
        ],
        &[
            "agentctl",
            "exec",
            "codex-1",
            "--",
            "bash",
            "-lc",
            "echo codex > /root/marker.txt",
        ],
        &[
            "agentctl",
            "exec",
            "claude-1",
            "--",
            "bash",
            "-lc",
            "test ! -e /root/marker.txt",
        ],
        &[
            "agentctl", "session", "create", "codex-1", "dev", "--", "bash",
        ],
        &[
            "agentctl", "session", "create", "codex-1", "codex", "--", "codex",
        ],
        &["agentctl", "session", "list", "codex-1"],
        &["agentctl", "export", "codex-1", "--type", "dpkg-delta"],
        &[
            "agentctl",
            "export",
            "codex-1",
            "--type",
            "rootfs-changed-paths",
        ],
        &["agentctl", "env", "stop", "codex-1"],
        &["agentctl", "env", "destroy", "codex-1"],
    ];

    for command in commands {
        let status = Command::new(command[0])
            .args(&command[1..])
            .status()
            .unwrap_or_else(|error| panic!("failed to execute {command:?}: {error}"));
        assert!(status.success(), "{command:?} exited with {status}");
    }
}
