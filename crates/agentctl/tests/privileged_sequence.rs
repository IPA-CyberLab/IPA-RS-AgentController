use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

#[test]
#[ignore = "requires privileged Btrfs/systemd-nspawn Project VM with /agentfs"]
fn goal_sequence_runs_in_privileged_project_vm() {
    require_privileged_test_environment();

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
    assert!(Path::new("/agentfs/envs/codex-1/logs/nspawn.log").exists());

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
    assert_btrfs_snapshot_of(
        "/agentfs/bases/base-001/rootfs",
        "/agentfs/envs/codex-1/rootfs",
    );
    assert_btrfs_snapshot_of(
        "/agentfs/bases/base-001/rootfs",
        "/agentfs/envs/claude-1/rootfs",
    );
    let codex_qgroup = btrfs_qgroup_id("/agentfs/envs/codex-1/rootfs");
    assert_child_cannot_see_project_vm_state("codex-1");

    assert_eq!(
        text(&["agentctl", "exec", "codex-1", "--", "sudo", "whoami"]).trim(),
        "root"
    );
    assert!(text(&["agentctl", "exec", "codex-1", "--", "tee", "--version"]).contains("tee"));
    run(&["agentctl", "exec", "codex-1", "--", "sudo", "apt", "update"]);
    run(&[
        "agentctl", "exec", "codex-1", "--", "sudo", "apt", "install", "-y", "ripgrep",
    ]);
    assert_file_contains(
        "/agentfs/envs/codex-1/logs/exec.log",
        "sudo apt install -y ripgrep",
    );
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
    assert!(sessions.contains("running"));
    assert_env_sessions("codex-1", &["dev"]);

    run(&[
        "agentctl", "session", "create", "codex-1", "codex", "--", "codex",
    ]);
    let sessions = text(&["agentctl", "session", "list", "codex-1"]);
    assert!(sessions.contains("codex"));
    assert!(sessions.contains("running"));
    assert_env_sessions("codex-1", &["dev", "codex"]);
    let _ = text(&["agentctl", "session", "logs", "codex-1", "codex"]);
    assert!(Path::new("/agentfs/envs/codex-1/logs/sessions/codex.log").exists());
    run(&["agentctl", "session", "detach", "codex-1", "codex"]);
    assert_session_running("codex-1", "codex");

    let dpkg_delta = text(&["agentctl", "export", "codex-1", "--type", "dpkg-delta"]);
    assert!(dpkg_delta.contains("ripgrep"));
    assert_file_contains("/agentfs/envs/codex-1/exports/dpkg-delta.txt", "ripgrep");
    let changed_paths = text(&[
        "agentctl",
        "export",
        "codex-1",
        "--type",
        "rootfs-changed-paths",
    ]);
    assert!(changed_paths.contains("/root/marker.txt"));
    assert_file_contains(
        "/agentfs/envs/codex-1/exports/rootfs-changed-paths.txt",
        "/root/marker.txt",
    );

    run(&["agentctl", "env", "stop", "codex-1"]);
    assert_file_contains("/agentfs/envs/codex-1/logs/lifecycle.log", "stopped");
    run(&["agentctl", "env", "destroy", "codex-1"]);
    assert!(!Path::new("/agentfs/envs/codex-1/rootfs").exists());
    assert!(!Path::new("/agentfs/envs/codex-1").exists());
    assert!(!Path::new("/etc/systemd/nspawn/af-codex-1.nspawn").exists());
    assert_btrfs_qgroup_removed(&codex_qgroup, "/agentfs");
    let claude_status = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(&claude_status, "claude-1", "base-001", "running");
}

fn require_privileged_test_environment() {
    assert_eq!(
        text(&["id", "-u"]).trim(),
        "0",
        "run this ignored integration test as root; it invokes chroot and btrfs inspection commands"
    );
    for program in [
        "agentctl",
        "agent-forkd",
        "btrfs",
        "machinectl",
        "systemd-nspawn",
        "systemd-run",
        "tmux",
    ] {
        run(&["bash", "-lc", &format!("command -v {program}")]);
    }
    assert_eq!(
        text(&["findmnt", "-n", "-o", "FSTYPE", "--target", "/"]).trim(),
        "btrfs",
        "/ must be on Btrfs"
    );
    assert_eq!(
        text(&["findmnt", "-n", "-o", "FSTYPE", "--target", "/agentfs"]).trim(),
        "btrfs",
        "/agentfs must be on Btrfs"
    );
    run(&["btrfs", "subvolume", "show", "/"]);
    run(&["systemctl", "is-active", "--quiet", "agent-forkd"]);
}

fn assert_env_status(status: &Value, id: &str, base_id: &str, state: &str) {
    assert_eq!(status["env"]["id"], id);
    assert_eq!(status["env"]["base_id"], base_id);
    assert_eq!(status["env"]["state"], state);
}

fn assert_env_sessions(env_id: &str, expected: &[&str]) {
    let status = json(&["agentctl", "env", "status", env_id]);
    let sessions = status["env"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("env {env_id} status did not contain sessions array"));

    for session_id in expected {
        assert!(
            sessions.iter().any(|value| value == session_id),
            "env {env_id} did not record session {session_id}; sessions={sessions:?}"
        );
    }
}

fn assert_session_running(env_id: &str, session_id: &str) {
    let sessions = text(&["agentctl", "session", "list", env_id]);

    assert!(
        sessions
            .lines()
            .any(|line| line.contains(session_id) && line.contains("running")),
        "session {session_id} in env {env_id} was not listed as running:\n{sessions}"
    );
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

fn assert_btrfs_snapshot_of(parent: &str, child: &str) {
    let parent_uuid = btrfs_subvolume_field(parent, "UUID");
    let child_parent_uuid = btrfs_subvolume_field(child, "Parent UUID");

    assert_eq!(
        child_parent_uuid, parent_uuid,
        "{child} was not recorded as a snapshot of {parent}"
    );
}

fn btrfs_qgroup_id(path: &str) -> String {
    format!("0/{}", btrfs_subvolume_field(path, "Subvolume ID"))
}

fn assert_btrfs_qgroup_removed(qgroup_id: &str, filesystem: &str) {
    let qgroups = text(&["btrfs", "qgroup", "show", "-reF", filesystem]);

    assert!(
        !qgroups
            .lines()
            .any(|line| line.split_whitespace().next() == Some(qgroup_id)),
        "{qgroup_id} still exists in qgroup output:\n{qgroups}"
    );
}

fn btrfs_subvolume_field(path: &str, field: &str) -> String {
    let prefix = format!("{field}:");
    for line in text(&["btrfs", "subvolume", "show", path]).lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix(&prefix) {
            return value.trim().to_string();
        }
    }
    panic!("btrfs subvolume show {path} did not contain field {field}");
}

fn assert_child_cannot_see_project_vm_state(env_id: &str) {
    run(&[
        "agentctl",
        "exec",
        env_id,
        "--",
        "bash",
        "-lc",
        "\
test ! -e /agentfs/bases && \
test ! -e /agentfs/envs && \
test ! -S /agentfs/runtime/sockets/agent-forkd.sock && \
test ! -S /run/docker.sock && \
test ! -S /var/run/docker.sock",
    ]);
}

fn assert_file_contains(path: &str, expected: &str) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("failed to read {path}: {error}");
    });
    assert!(
        text.contains(expected),
        "{path} did not contain {expected:?}"
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
