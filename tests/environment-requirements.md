# Privileged Test Environment Requirements

This project has one ignored privileged integration test:

```bash
cargo test -p agentctl --test privileged_sequence -- --ignored
```

Run it only on a disposable Linux machine or VM. The test creates and destroys
Btrfs subvolumes, starts systemd-nspawn machines, installs packages with apt,
and writes system configuration under `/etc/systemd`.

## Access

- SSH access for the tester user.
- A root SSH session, or passwordless `sudo` that can run the whole test under
  root.
- Network access from the machine to apt repositories and GitHub.
- Enough disk space for two rootfs snapshots and package installation. Use at
  least 120 GiB free on the Btrfs filesystem if possible.

## Operating System

- Linux with systemd as PID 1.
- Debian or Ubuntu is the expected target because the test uses `apt`,
  `apt-get`, `dpkg`, and `sudo`.
- cgroup v2 enabled.
- User namespaces enabled.
- systemd-machined and systemd-networkd available.

Useful checks:

```bash
test "$(ps -p 1 -o comm=)" = systemd
stat -fc %T /sys/fs/cgroup
sysctl kernel.unprivileged_userns_clone
systemctl status systemd-machined --no-pager
systemctl status systemd-networkd --no-pager
```

`stat -fc %T /sys/fs/cgroup` should report `cgroup2fs`.

## Required Host Packages

Install these on the test machine before running the integration test:

```bash
sudo apt update
sudo apt install -y \
  btrfs-progs \
  systemd-container \
  tmux \
  sudo \
  curl \
  git \
  build-essential \
  pkg-config
```

Rust must also be available:

```bash
command -v cargo
command -v rustc
```

The test expects `agentctl` and `agent-forkd` to be built from this repository.

## Btrfs Layout

Both `/` and `/agentfs` must be on Btrfs, and `/` must be a Btrfs subvolume.
The implementation intentionally does not fall back to rsync or full copies.

Required checks:

```bash
findmnt -n -o FSTYPE --target /
findmnt -n -o FSTYPE --target /agentfs
sudo btrfs subvolume show /
sudo btrfs filesystem show /agentfs
```

Both `findmnt` commands must print `btrfs`, and `btrfs subvolume show /` must
succeed.

Create `/agentfs` on Btrfs before the test. It may be a mount point or a Btrfs
subvolume on the same filesystem:

```bash
sudo mkdir -p /agentfs
sudo btrfs quota enable /agentfs
```

The test will create and remove:

```text
/agentfs/bases/base-001
/agentfs/envs/codex-1
/agentfs/envs/claude-1
/agentfs/runtime
/etc/systemd/nspawn/af-codex-1.nspawn
/etc/systemd/nspawn/af-claude-1.nspawn
/etc/systemd/network/80-agent-forkd-private-nat.network
```

Use a fresh machine or remove stale state before running:

```bash
sudo machinectl terminate af-codex-1 af-claude-1 || true
sudo rm -f /etc/systemd/nspawn/af-codex-1.nspawn
sudo rm -f /etc/systemd/nspawn/af-claude-1.nspawn
sudo rm -f /etc/systemd/network/80-agent-forkd-private-nat.network
sudo btrfs subvolume delete /agentfs/envs/codex-1/rootfs 2>/dev/null || true
sudo btrfs subvolume delete /agentfs/envs/claude-1/rootfs 2>/dev/null || true
sudo btrfs subvolume delete /agentfs/bases/base-001/rootfs 2>/dev/null || true
sudo rm -rf /agentfs/bases/base-001 /agentfs/envs/codex-1 /agentfs/envs/claude-1
```

## Child Rootfs Requirements

The base rootfs is created from `/`, so the host rootfs must contain the tools
that child environments need:

```bash
test -x /bin/bash
command -v sudo
command -v apt || command -v apt-get
command -v tmux
command -v tee
```

The integration sequence also installs and verifies `ripgrep` inside `codex-1`,
so apt egress must work from the child nspawn private NAT network.

## Build And Install

From the repository checkout:

```bash
cargo build --release
sudo install -m 0755 target/release/agent-forkd /usr/local/bin/agent-forkd
sudo install -m 0755 target/release/agentctl /usr/local/bin/agentctl
sudo install -d -m 0755 /etc/agent-forkd
sudo install -m 0644 packaging/agent-forkd/config.json /etc/agent-forkd/config.json
sudo install -m 0644 packaging/systemd/agent-forkd.service /etc/systemd/system/agent-forkd.service
sudo systemctl daemon-reload
sudo systemctl enable --now agent-forkd
```

Confirm the daemon socket exists:

```bash
test -S /agentfs/runtime/sockets/agent-forkd.sock
```

## Running The Test

Run the normal local gates first:

```bash
cargo fmt -- --check
cargo test --quiet
cargo clippy --all-targets -- -D warnings
git diff --check
```

Then run the privileged sequence as root. This is required because the test
process itself invokes `chroot`, `btrfs subvolume show`, `btrfs qgroup show`,
and other host inspection commands:

```bash
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  cargo test -p agentctl --test privileged_sequence -- --ignored --nocapture
```

If running as root directly, the shorter command is also fine:

```bash
cargo test -p agentctl --test privileged_sequence -- --ignored --nocapture
```

The shell/attach parts of the full manual sequence are documented in
`tests/goal-sequence.md`. The automated Rust test avoids requiring a human to
interactively detach from tmux, but it still validates session creation, logs,
detach behavior, exports, destroy, Btrfs snapshot properties, qgroup cleanup,
and sibling env isolation.

## Expected Final State

After a successful run:

- `codex-1` is destroyed.
- `claude-1` is still running.
- `/agentfs/envs/codex-1` is gone.
- the qgroup for `codex-1` is gone.
- `/agentfs/envs/claude-1/rootfs` remains a writable Btrfs subvolume.

Optional cleanup:

```bash
agentctl env destroy claude-1 || true
sudo machinectl terminate af-claude-1 || true
sudo rm -rf /agentfs/bases/base-001 /agentfs/envs/claude-1
```
