# IPA-RS Isolated Agent

`agent-forkd` and `agentctl` manage forked privileged development environments inside one Kata-backed Project VM.

The implementation uses:

- Btrfs read-only base snapshots and writable child snapshots
- Btrfs qgroup quotas per child rootfs
- systemd-nspawn machines with `PrivateUsers=yes` and private networking
- tmux-backed persistent PTY sessions
- JSON metadata under `/agentfs`
- a Unix socket API at `/agentfs/runtime/sockets/agent-forkd.sock`

## Build

```bash
cargo build --release
sudo install -m 0755 target/release/agent-forkd /usr/local/bin/agent-forkd
sudo install -m 0755 target/release/agentctl /usr/local/bin/agentctl
sudo install -m 0644 packaging/systemd/agent-forkd.service /etc/systemd/system/agent-forkd.service
sudo systemctl daemon-reload
sudo systemctl enable --now agent-forkd
```

## Requirements

The Project VM must provide Linux, Btrfs, `btrfs-progs`, systemd, `systemd-nspawn`, `machinectl`, cgroup v2, user namespaces, and `tmux`.

`/agentfs` must be on a Btrfs filesystem. `agentctl base freeze --from /` requires `/` itself to be a Btrfs subvolume. The implementation intentionally fails when that is not true and does not fall back to a full copy.

## Usage

```bash
agentctl init --agentfs /agentfs
agentctl base freeze --name base-001 --from /

agentctl env create codex-1 --from base-001 --profile privileged-dev
agentctl env create claude-1 --from base-001 --profile privileged-dev
agentctl env start codex-1
agentctl env start claude-1

agentctl exec codex-1 -- sudo apt update
agentctl exec codex-1 -- sudo apt install -y ripgrep
agentctl session create codex-1 dev -- bash
agentctl session attach codex-1 dev

agentctl env list
agentctl env status codex-1
agentctl session list codex-1
agentctl export codex-1 --type dpkg-delta
agentctl export codex-1 --type rootfs-changed-paths
agentctl env stop codex-1
agentctl env destroy codex-1
```

## Metadata Layout

```text
/agentfs
  /bases/<base-id>/manifest.json
  /bases/<base-id>/dpkg.list
  /envs/<env-id>/meta.json
  /envs/<env-id>/sessions/<session-id>.json
  /envs/<env-id>/logs/exec.log
  /envs/<env-id>/logs/sessions/<session-id>.log
  /runtime/sockets/agent-forkd.sock
```

JSON schemas live in `schemas/`.

## Security Model

Child environments are not separate VMs. They are privileged development roots inside the Project VM and rely on the outer Kata VM for the kernel boundary. `agent-forkd` still configures nspawn private users and private networking, does not bind `/agentfs` into children, and keeps base and sibling rootfs trees outside the child view.

## Test Notes

Unit tests cover command generation, schema-adjacent metadata behavior, and export delta logic. Full integration tests require a privileged Btrfs/systemd-nspawn Project VM and should execute the sequence in `docs/goal.md`.
