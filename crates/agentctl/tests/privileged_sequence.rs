use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

#[test]
#[ignore = "requires privileged Btrfs/systemd-nspawn Project VM with /agentfs"]
fn goal_sequence_runs_in_privileged_project_vm() {
    run(&["agentctl", "init", "--agentfs", "/agentfs"]);
    run(&[
        "agentctl", "base", "freeze", "--name", "base-001", "--from", "/",
    ]);
    assert!(Path::new("/agentfs/bases/base-001/rootfs").exists());
    assert!(Path::new("/agentfs/bases/base-001/manifest.json").exists());
    assert!(Path::new("/agentfs/bases/base-001/dpkg.list").exists());
    assert_btrfs_subvolume("/agentfs/bases/base-001/rootfs");
    assert_btrfs_readonly("/agentfs/bases/base-001/rootfs", true);

    run(&[
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
    ]);
    run(&[
        "agentctl",
        "env",
        "create",
        "claude-1",
        "--from",
        "base-001",
        "--profile",
        "privileged-dev",
    ]);
    run(&["agentctl", "env", "start", "codex-1"]);
    run(&["agentctl", "env", "start", "claude-1"]);

    let codex_status = json(&["agentctl", "env", "status", "codex-1"]);
    let claude_status = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(&codex_status, "codex-1", "base-001", "running");
    assert_env_status(&claude_status, "claude-1", "base-001", "running");
    assert_ne!(
        codex_status["env"]["rootfs_path"],
        claude_status["env"]["rootfs_path"]
    );
    assert_btrfs_subvolume("/agentfs/envs/codex-1/rootfs");
    assert_btrfs_subvolume("/agentfs/envs/claude-1/rootfs");
    assert_btrfs_readonly("/agentfs/envs/codex-1/rootfs", false);
    assert_btrfs_readonly("/agentfs/envs/claude-1/rootfs", false);

    run(&["agentctl", "exec", "codex-1", "--", "sudo", "apt", "update"]);
    run(&[
        "agentctl", "exec", "codex-1", "--", "sudo", "apt", "install", "-y", "ripgrep",
    ]);
    assert!(text(&["agentctl", "exec", "codex-1", "--", "rg", "--version"]).contains("ripgrep"));
    assert_eq!(
        text(&[
            "agentctl",
            "exec",
            "claude-1",
            "--",
            "bash",
            "-lc",
            "command -v rg || true",
        ])
        .trim(),
        ""
    );
    assert!(!text(&[
        "chroot",
        "/agentfs/bases/base-001/rootfs",
        "bash",
        "-lc",
        "command -v rg || true"
    ])
    .contains("rg"));

    run(&[
        "agentctl",
        "exec",
        "codex-1",
        "--",
        "bash",
        "-lc",
        "echo codex > /root/marker.txt",
    ]);
    run(&[
        "agentctl",
        "exec",
        "claude-1",
        "--",
        "bash",
        "-lc",
        "test ! -e /root/marker.txt",
    ]);

    run(&[
        "agentctl", "session", "create", "codex-1", "dev", "--", "bash",
    ]);
    let sessions = text(&["agentctl", "session", "list", "codex-1"]);
    assert!(sessions.contains("dev"));
    assert!(sessions.contains("Running"));

    run(&[
        "agentctl", "session", "create", "codex-1", "codex", "--", "codex",
    ]);
    let sessions = text(&["agentctl", "session", "list", "codex-1"]);
    assert!(sessions.contains("codex"));
    assert!(sessions.contains("Running"));
    let _ = text(&["agentctl", "session", "logs", "codex-1", "codex"]);
    assert!(Path::new("/agentfs/envs/codex-1/logs/sessions/codex.log").exists());
    run(&["agentctl", "session", "detach", "codex-1", "codex"]);

    let dpkg_delta = text(&["agentctl", "export", "codex-1", "--type", "dpkg-delta"]);
    assert!(dpkg_delta.contains("ripgrep"));
    let changed_paths = text(&[
        "agentctl",
        "export",
        "codex-1",
        "--type",
        "rootfs-changed-paths",
    ]);
    assert!(changed_paths.contains("/root/marker.txt"));

    run(&["agentctl", "env", "stop", "codex-1"]);
    run(&["agentctl", "env", "destroy", "codex-1"]);
    assert!(!Path::new("/agentfs/envs/codex-1/rootfs").exists());
    let claude_status = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(&claude_status, "claude-1", "base-001", "running");
}

fn assert_env_status(status: &Value, id: &str, base_id: &str, state: &str) {
    assert_eq!(status["env"]["id"], id);
    assert_eq!(status["env"]["base_id"], base_id);
    assert_eq!(status["env"]["state"], state);
}

fn assert_btrfs_subvolume(path: &str) {
    run(&["btrfs", "subvolume", "show", path]);
}

fn assert_btrfs_readonly(path: &str, readonly: bool) {
    let expected = if readonly { "ro=true" } else { "ro=false" };
    assert_eq!(
        text(&["btrfs", "property", "get", "-ts", path, "ro"]).trim(),
        expected
    );
}

fn json(command: &[&str]) -> Value {
    serde_json::from_str(&text(command)).unwrap_or_else(|error| {
        panic!("failed to parse json from {command:?}: {error}");
    })
}

fn text(command: &[&str]) -> String {
    let output = run(command);
    String::from_utf8(output.stdout).unwrap_or_else(|error| {
        panic!("stdout from {command:?} was not utf-8: {error}");
    })
}

fn run(command: &[&str]) -> Output {
    let output = Command::new(command[0])
        .args(&command[1..])
        .output()
        .unwrap_or_else(|error| panic!("failed to execute {command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?} exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
