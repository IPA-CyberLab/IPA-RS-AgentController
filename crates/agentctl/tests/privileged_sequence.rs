use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Output};

#[test]
#[ignore = "requires privileged Btrfs/systemd-nspawn Project VM with /agentfs"]
fn goal_sequence_runs_in_privileged_project_vm() {
    require_privileged_test_environment();

    run(&["agentctl", "init", "--agentfs", "/agentfs"]);
    assert_agentfs_layout_initialized();
    run(&[
        "agentctl", "base", "freeze", "--name", "base-001", "--from", "/",
    ]);
    assert!(Path::new("/agentfs/bases/base-001/rootfs").exists());
    assert!(Path::new("/agentfs/bases/base-001/manifest.json").exists());
    assert!(Path::new("/agentfs/bases/base-001/dpkg.list").exists());
    assert!(Path::new("/agentfs/bases/base-001/created_at").exists());
    assert_btrfs_subvolume("/agentfs/bases/base-001/rootfs");
    assert_btrfs_readonly("/agentfs/bases/base-001/rootfs", true);
    assert_base_metadata("base-001");
    assert_dpkg_manifest("base-001");
    assert_base_runtime_paths_scrubbed("base-001");

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
    assert_env_metadata("codex-1", "base-001", "created");
    assert_env_metadata("claude-1", "base-001", "created");
    run(&["agentctl", "env", "start", "codex-1"]);
    run(&["agentctl", "env", "start", "claude-1"]);
    assert_nspawn_config_for("codex-1");
    assert_private_nat_network_config();
    assert!(Path::new("/agentfs/envs/codex-1/logs/nspawn.log").exists());
    assert_file_contains("/agentfs/envs/codex-1/logs/agent-forkd.log", "env created");
    assert_file_contains("/agentfs/envs/codex-1/logs/lifecycle.log", "running");

    let codex_status = json(&["agentctl", "env", "status", "codex-1"]);
    let claude_status = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(&codex_status, "codex-1", "base-001", "running");
    assert_env_status(&claude_status, "claude-1", "base-001", "running");
    assert_env_list_contains_running(&["codex-1", "claude-1"]);
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
    assert_eq!(
        text(&["agentctl", "exec", "codex-1", "--", "hostname"]).trim(),
        "af-codex-1"
    );
    assert_eq!(
        text(&["agentctl", "exec", "claude-1", "--", "hostname"]).trim(),
        "af-claude-1"
    );
    assert_runtime_namespaces_are_isolated("codex-1", "claude-1");
    let codex_qgroup = btrfs_qgroup_id("/agentfs/envs/codex-1/rootfs");
    assert_btrfs_qgroup_has_referenced_limit(&codex_qgroup, "/agentfs/envs/codex-1/rootfs");
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
        "agentctl",
        "exec",
        "codex-1",
        "--",
        "bash",
        "-lc",
        "mkdir -p /workspace && \
         cd /workspace && \
         git init --quiet && \
         git config user.email test@example.invalid && \
         git config user.name 'Agent Forkd Test' && \
         printf 'old\n' > README.md && \
         git add README.md && \
         git commit --quiet -m initial && \
         printf 'new\n' > README.md",
    ]);
    let workspace_diff = text(&["agentctl", "diff", "codex-1"]);
    assert!(workspace_diff.contains("-old"));
    assert!(workspace_diff.contains("+new"));

    run(&[
        "agentctl", "session", "create", "codex-1", "dev", "--", "bash",
    ]);
    let sessions = text(&["agentctl", "session", "list", "codex-1"]);
    assert!(sessions.contains("dev"));
    assert!(sessions.contains("running"));
    assert_env_sessions("codex-1", &["dev"]);
    assert_session_metadata("codex-1", "dev", "bash", "running");
    assert_shell_request_creates_persistent_session("codex-1");
    run(&[
        "agentctl",
        "session",
        "create",
        "codex-1",
        "logger",
        "--",
        "bash",
        "-lc",
        "printf 'session-log-sentinel\n'; sleep infinity",
    ]);
    assert_env_sessions("codex-1", &["dev", "logger", "shell"]);
    assert_session_metadata(
        "codex-1",
        "logger",
        "bash -lc 'printf '\\''session-log-sentinel\n'\\''; sleep infinity'",
        "running",
    );
    assert_session_logs_contain("codex-1", "logger", "session-log-sentinel");

    run(&[
        "agentctl", "session", "create", "codex-1", "codex", "--", "codex",
    ]);
    let sessions = text(&["agentctl", "session", "list", "codex-1"]);
    assert!(sessions.contains("codex"));
    assert!(sessions.contains("running"));
    assert_env_sessions("codex-1", &["dev", "codex"]);
    assert_session_metadata("codex-1", "codex", "codex", "running");
    let _ = text(&["agentctl", "session", "logs", "codex-1", "codex"]);
    assert!(Path::new("/agentfs/envs/codex-1/logs/sessions/codex.log").exists());
    run(&["agentctl", "session", "detach", "codex-1", "codex"]);
    assert_session_running("codex-1", "codex");
    assert_file_contains(
        "/agentfs/envs/codex-1/logs/lifecycle.log",
        "session codex detached",
    );

    let dpkg_delta = text(&["agentctl", "export", "codex-1", "--type", "dpkg-delta"]);
    assert!(dpkg_delta.contains("ripgrep"));
    assert_file_contains("/agentfs/envs/codex-1/exports/dpkg-delta.txt", "ripgrep");
    let workspace_patch = text(&["agentctl", "export", "codex-1", "--type", "workspace-patch"]);
    assert!(workspace_patch.contains("-old"));
    assert!(workspace_patch.contains("+new"));
    assert_file_contains(
        "/agentfs/envs/codex-1/exports/workspace-patch.patch",
        "+new",
    );
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
    assert_file_contains(
        "/agentfs/envs/codex-1/logs/lifecycle.log",
        "exported /agentfs/envs/codex-1/exports/rootfs-changed-paths.txt",
    );

    run(&["agentctl", "env", "stop", "codex-1"]);
    let stopped_status = json(&["agentctl", "env", "status", "codex-1"]);
    assert_env_status(&stopped_status, "codex-1", "base-001", "stopped");
    assert_file_contains("/agentfs/envs/codex-1/logs/lifecycle.log", "stopped");
    assert_env_artifacts_persist_after_stop("codex-1");
    assert_session_logs_contain("codex-1", "logger", "session-log-sentinel");
    restart_agent_forkd_daemon();
    let stopped_after_daemon_restart = json(&["agentctl", "env", "status", "codex-1"]);
    assert_env_status(
        &stopped_after_daemon_restart,
        "codex-1",
        "base-001",
        "stopped",
    );
    assert_env_artifacts_persist_after_stop("codex-1");
    assert_session_logs_contain("codex-1", "logger", "session-log-sentinel");
    let claude_status_after_daemon_restart = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(
        &claude_status_after_daemon_restart,
        "claude-1",
        "base-001",
        "running",
    );
    run(&["agentctl", "env", "destroy", "codex-1"]);
    assert!(!Path::new("/agentfs/envs/codex-1/rootfs").exists());
    assert!(!Path::new("/agentfs/envs/codex-1").exists());
    assert!(!Path::new("/etc/systemd/nspawn/af-codex-1.nspawn").exists());
    assert_btrfs_qgroup_removed(&codex_qgroup, "/agentfs");
    let claude_status = json(&["agentctl", "env", "status", "claude-1"]);
    assert_env_status(&claude_status, "claude-1", "base-001", "running");
    assert_eq!(
        text(&[
            "agentctl",
            "exec",
            "claude-1",
            "--",
            "bash",
            "-lc",
            "test ! -e /root/marker.txt && echo sibling-ok",
        ])
        .trim(),
        "sibling-ok"
    );
}

fn require_privileged_test_environment() {
    assert_eq!(
        text(&["id", "-u"]).trim(),
        "0",
        "run this ignored integration test as root; it invokes chroot and btrfs inspection commands"
    );
    assert_eq!(
        text(&["ps", "-p", "1", "-o", "comm="]).trim(),
        "systemd",
        "PID 1 must be systemd"
    );
    assert_eq!(
        text(&["stat", "-fc", "%T", "/sys/fs/cgroup"]).trim(),
        "cgroup2fs",
        "/sys/fs/cgroup must be cgroup v2"
    );
    let userns = text(&[
        "bash",
        "-lc",
        "sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || true",
    ]);
    assert!(
        userns.trim().is_empty() || userns.trim() == "1",
        "kernel.unprivileged_userns_clone must be 1 or unavailable, got {userns:?}"
    );
    for program in [
        "agentctl",
        "agent-forkd",
        "btrfs",
        "cargo",
        "chroot",
        "dpkg",
        "findmnt",
        "git",
        "machinectl",
        "readlink",
        "rustc",
        "sudo",
        "systemctl",
        "systemd-nspawn",
        "systemd-run",
        "tee",
        "tmux",
        "codex",
    ] {
        run(&["bash", "-lc", &format!("command -v {program}")]);
    }
    assert!(
        command_available("apt") || command_available("apt-get"),
        "apt or apt-get must be available"
    );
    run(&["bash", "-lc", "test -x /bin/bash"]);
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
    run(&["systemctl", "cat", "systemd-machined"]);
    run(&["systemctl", "is-active", "--quiet", "systemd-networkd"]);
    run(&["systemctl", "is-active", "--quiet", "agent-forkd"]);
    run(&[
        "bash",
        "-lc",
        "test -S /agentfs/runtime/sockets/agent-forkd.sock",
    ]);
}

fn assert_env_status(status: &Value, id: &str, base_id: &str, state: &str) {
    assert_eq!(status["env"]["id"], id);
    assert_eq!(status["env"]["base_id"], base_id);
    assert_eq!(status["env"]["state"], state);
}

fn assert_base_metadata(base_id: &str) {
    let metadata = json_file(&format!("/agentfs/bases/{base_id}/manifest.json"));
    let created_at = std::fs::read_to_string(format!("/agentfs/bases/{base_id}/created_at"))
        .unwrap_or_else(|error| panic!("failed to read base created_at file: {error}"));
    assert_eq!(metadata["id"], base_id);
    assert_eq!(
        metadata["rootfs_path"],
        format!("/agentfs/bases/{base_id}/rootfs")
    );
    assert_eq!(metadata["readonly"], true);
    assert_eq!(metadata["source"], "/");
    assert_eq!(
        metadata["dpkg_manifest"],
        format!("/agentfs/bases/{base_id}/dpkg.list")
    );
    assert!(
        metadata["created_at"].as_str().is_some(),
        "base metadata omitted created_at: {metadata}"
    );
    assert_eq!(metadata["created_at"].as_str().unwrap(), created_at);
}

fn assert_dpkg_manifest(base_id: &str) {
    let path = format!("/agentfs/bases/{base_id}/dpkg.list");
    let manifest = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read base dpkg manifest {path}: {error}"));
    let entries: Vec<_> = manifest
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(!entries.is_empty(), "{path} was empty");
    assert!(
        entries
            .iter()
            .any(|line| line.split_whitespace().next() == Some("bash")),
        "{path} did not include bash package:\n{manifest}"
    );
    for line in entries.iter().take(20) {
        assert!(
            line.split_whitespace().count() >= 2,
            "dpkg manifest line lacked package/version: {line:?}"
        );
    }
}

fn assert_base_runtime_paths_scrubbed(base_id: &str) {
    let rootfs = format!("/agentfs/bases/{base_id}/rootfs");
    for rel in ["proc", "sys", "dev", "run", "tmp", "agentfs"] {
        let path = format!("{rootfs}/{rel}");
        assert!(Path::new(&path).is_dir(), "{path} was not recreated");
    }
    assert_eq!(
        std::fs::metadata(format!("{rootfs}/tmp"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o1777
    );
    for rel in [
        "agentfs/bases",
        "agentfs/envs",
        "agentfs/cache",
        "agentfs/runtime",
    ] {
        let path = format!("{rootfs}/{rel}");
        assert!(
            !Path::new(&path).exists(),
            "{path} leaked host agentfs state into base"
        );
    }
}

fn assert_env_metadata(env_id: &str, base_id: &str, state: &str) {
    let metadata = json_file(&format!("/agentfs/envs/{env_id}/meta.json"));
    assert_eq!(metadata["id"], env_id);
    assert_eq!(metadata["base_id"], base_id);
    assert_eq!(
        metadata["rootfs_path"],
        format!("/agentfs/envs/{env_id}/rootfs")
    );
    assert_eq!(metadata["machine_name"], format!("af-{env_id}"));
    assert_eq!(metadata["state"], state);
    assert_eq!(metadata["profile"], "privileged-dev");
    assert_eq!(metadata["limits"]["cpu_max"], "400%");
    assert_eq!(metadata["limits"]["memory_max"], "16G");
    assert_eq!(metadata["limits"]["pids_max"], 4096);
    assert_eq!(metadata["limits"]["disk_max"], "100G");
    assert_eq!(metadata["limits"]["network"], "private-nat");
    assert_eq!(metadata["limits"]["idle_timeout"], "0");
    assert_eq!(metadata["limits"]["max_runtime"], "0");
    assert!(metadata["sessions"].as_array().unwrap().is_empty());
    assert!(
        metadata["created_at"].as_str().is_some(),
        "env metadata omitted created_at: {metadata}"
    );
}

fn assert_session_metadata(env_id: &str, session_id: &str, command: &str, state: &str) {
    let metadata = json_file(&format!(
        "/agentfs/envs/{env_id}/sessions/{session_id}.json"
    ));
    assert_eq!(metadata["id"], session_id);
    assert_eq!(metadata["env_id"], env_id);
    assert_eq!(metadata["command"], command);
    assert_eq!(metadata["state"], state);
    assert_eq!(metadata["type"], "pty");
    assert_eq!(
        metadata["log_path"],
        format!("/agentfs/envs/{env_id}/logs/sessions/{session_id}.log")
    );
    assert!(
        metadata["created_at"].as_str().is_some(),
        "session metadata omitted created_at: {metadata}"
    );
}

fn assert_session_logs_contain(env_id: &str, session_id: &str, needle: &str) {
    let logs = text(&["agentctl", "session", "logs", env_id, session_id]);
    assert!(
        logs.contains(needle),
        "session {session_id} logs for {env_id} did not contain {needle:?}:\n{logs}"
    );
}

fn restart_agent_forkd_daemon() {
    run(&["systemctl", "restart", "agent-forkd"]);
    run(&[
        "bash",
        "-lc",
        "for _ in $(seq 1 50); do \
           test -S /agentfs/runtime/sockets/agent-forkd.sock && exit 0; \
           sleep 0.1; \
         done; \
         exit 1",
    ]);
}

fn assert_env_artifacts_persist_after_stop(env_id: &str) {
    for path in [
        format!("/agentfs/envs/{env_id}/rootfs"),
        format!("/agentfs/envs/{env_id}/meta.json"),
        format!("/agentfs/envs/{env_id}/sessions/dev.json"),
        format!("/agentfs/envs/{env_id}/sessions/logger.json"),
        format!("/agentfs/envs/{env_id}/sessions/codex.json"),
        format!("/agentfs/envs/{env_id}/logs/agent-forkd.log"),
        format!("/agentfs/envs/{env_id}/logs/lifecycle.log"),
        format!("/agentfs/envs/{env_id}/logs/exec.log"),
        format!("/agentfs/envs/{env_id}/logs/nspawn.log"),
        format!("/agentfs/envs/{env_id}/logs/sessions/logger.log"),
        format!("/agentfs/envs/{env_id}/logs/sessions/codex.log"),
        format!("/agentfs/envs/{env_id}/exports/dpkg-delta.txt"),
        format!("/agentfs/envs/{env_id}/exports/workspace-patch.patch"),
        format!("/agentfs/envs/{env_id}/exports/rootfs-changed-paths.txt"),
    ] {
        assert!(
            Path::new(&path).exists(),
            "{path} did not persist after stop"
        );
    }
    assert_file_contains(
        &format!("/agentfs/envs/{env_id}/exports/rootfs-changed-paths.txt"),
        "/root/marker.txt",
    );
    assert_file_contains(
        &format!("/agentfs/envs/{env_id}/exports/dpkg-delta.txt"),
        "ripgrep",
    );
}

fn assert_shell_request_creates_persistent_session(env_id: &str) {
    let response = daemon_request(serde_json::json!({
        "type": "shell",
        "id": env_id
    }));
    assert_eq!(response["type"], "attach");
    assert_eq!(response["machine_name"], format!("af-{env_id}"));
    assert_eq!(response["session_id"], "shell");
    assert_session_metadata(env_id, "shell", "/bin/bash", "running");
    assert_env_sessions(env_id, &["dev", "shell"]);
}

fn assert_agentfs_layout_initialized() {
    for dir in [
        "/agentfs/bases",
        "/agentfs/envs",
        "/agentfs/cache/apt",
        "/agentfs/cache/compiler",
        "/agentfs/cache/package",
        "/agentfs/cache/ddc",
        "/agentfs/runtime/pty",
        "/agentfs/runtime/machines",
        "/agentfs/runtime/sockets",
    ] {
        assert!(Path::new(dir).is_dir(), "{dir} was not initialized");
    }
}

fn assert_env_list_contains_running(env_ids: &[&str]) {
    let list = text(&["agentctl", "env", "list"]);
    assert!(list.contains("ENV"), "env list omitted ENV header:\n{list}");
    assert!(
        list.contains("BASE"),
        "env list omitted BASE header:\n{list}"
    );
    assert!(
        list.contains("STATE"),
        "env list omitted STATE header:\n{list}"
    );
    assert!(
        list.contains("DISK_USED"),
        "env list omitted DISK_USED header:\n{list}"
    );
    assert!(
        list.contains("SESSIONS"),
        "env list omitted SESSIONS header:\n{list}"
    );

    for env_id in env_ids {
        assert!(
            list.lines()
                .any(|line| line.contains(env_id) && line.contains("running")),
            "env list did not show {env_id} as running:\n{list}"
        );
    }
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

fn assert_nspawn_config_for(env_id: &str) {
    let path = format!("/etc/systemd/nspawn/af-{env_id}.nspawn");
    assert_file_contains(&path, "Boot=yes");
    assert_file_contains(&path, "PrivateUsers=yes");
    assert_file_contains(&path, &format!("Hostname=af-{env_id}"));
    assert_file_contains(&path, "ReadOnly=no");
    assert_file_contains(&path, "Inaccessible=/agentfs");
    assert_file_contains(&path, "Inaccessible=/run/agent-forkd.sock");
    assert_file_contains(&path, "Inaccessible=/run/docker.sock");
    assert_file_contains(&path, "Inaccessible=/var/run/docker.sock");
    assert_file_contains(&path, "VirtualEthernet=yes");
    assert_file_contains(&path, "Zone=agent-forkd");
}

fn assert_private_nat_network_config() {
    let path = "/etc/systemd/network/80-agent-forkd-private-nat.network";
    assert_file_contains(path, "Name=vz-agent-forkd");
    assert_file_contains(path, "Address=10.77.0.1/24");
    assert_file_contains(path, "DHCPServer=yes");
    assert_file_contains(path, "IPMasquerade=ipv4");
    assert_file_contains(path, "IPForward=ipv4");
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

fn assert_btrfs_qgroup_has_referenced_limit(qgroup_id: &str, path: &str) {
    let qgroups = text(&["btrfs", "qgroup", "show", "-breF", path]);
    let mut lines = qgroups.lines();
    let header = lines
        .next()
        .unwrap_or_else(|| panic!("qgroup output for {path} was empty"));
    let headers = header.split_whitespace().collect::<Vec<_>>();
    let qgroup_index = headers
        .iter()
        .position(|field| *field == "qgroupid")
        .unwrap_or_else(|| panic!("qgroup output for {path} omitted qgroupid:\n{qgroups}"));
    let max_rfer_index = headers
        .iter()
        .position(|field| *field == "max_rfer")
        .unwrap_or_else(|| panic!("qgroup output for {path} omitted max_rfer:\n{qgroups}"));

    for line in lines {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.get(qgroup_index) != Some(&qgroup_id) {
            continue;
        }
        let max_rfer = fields
            .get(max_rfer_index)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        assert!(
            max_rfer > 0,
            "qgroup {qgroup_id} did not have a referenced limit:\n{qgroups}"
        );
        return;
    }
    panic!("qgroup {qgroup_id} was not present in qgroup output:\n{qgroups}");
}

fn assert_runtime_namespaces_are_isolated(left_env_id: &str, right_env_id: &str) {
    let host_network = text(&["readlink", "/proc/self/ns/net"]);
    let left_network = text(&[
        "agentctl",
        "exec",
        left_env_id,
        "--",
        "readlink",
        "/proc/self/ns/net",
    ]);
    let right_network = text(&[
        "agentctl",
        "exec",
        right_env_id,
        "--",
        "readlink",
        "/proc/self/ns/net",
    ]);
    assert_ne!(
        host_network.trim(),
        left_network.trim(),
        "{left_env_id} reused the host network namespace"
    );
    assert_ne!(
        left_network.trim(),
        right_network.trim(),
        "{left_env_id} and {right_env_id} shared a network namespace"
    );

    let host_user = text(&["readlink", "/proc/self/ns/user"]);
    let left_user = text(&[
        "agentctl",
        "exec",
        left_env_id,
        "--",
        "readlink",
        "/proc/self/ns/user",
    ]);
    assert_ne!(
        host_user.trim(),
        left_user.trim(),
        "{left_env_id} reused the host user namespace"
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

fn command_available(program: &str) -> bool {
    Command::new("bash")
        .args(["-lc", &format!("command -v {program}")])
        .status()
        .unwrap_or_else(|error| panic!("failed to inspect command {program}: {error}"))
        .success()
}

fn json(command: &[&str]) -> Value {
    serde_json::from_str(&text(command)).unwrap_or_else(|error| {
        panic!("failed to parse json from {command:?}: {error}");
    })
}

fn json_file(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("failed to read json file {path}: {error}");
    });
    serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!("failed to parse json file {path}: {error}");
    })
}

fn daemon_request(request: Value) -> Value {
    let mut stream = UnixStream::connect("/agentfs/runtime/sockets/agent-forkd.sock")
        .unwrap_or_else(|error| {
            panic!("failed to connect to agent-forkd socket: {error}");
        });
    writeln!(stream, "{request}").unwrap_or_else(|error| {
        panic!("failed to write daemon request {request}: {error}");
    });
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .unwrap_or_else(|error| {
            panic!("failed to read daemon response for {request}: {error}");
        });
    serde_json::from_str(&response).unwrap_or_else(|error| {
        panic!("failed to parse daemon response {response:?}: {error}");
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
