# IPA-RS Isolated Agent

`agent-forkd` and `agentctl` manage forked privileged development environments inside one Kata-backed Project VM.

The implementation uses:

- Btrfs read-only base snapshots and writable child snapshots
- Btrfs qgroup quotas per child rootfs
- systemd-nspawn machines with `PrivateUsers=yes` and private networking or private NAT
- tmux-backed persistent PTY sessions running inside each child machine
- JSON metadata under `/agentfs`
- a Unix socket API at `/agentfs/runtime/sockets/agent-forkd.sock`

## Build

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

## Requirements

The Project VM must provide Linux, Btrfs, `btrfs-progs`, systemd, `systemd-nspawn`, `machinectl`, `systemd-networkd`, cgroup v2, user namespaces, and `tmux`.

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
agentctl session detach codex-1 dev
agentctl session logs codex-1 dev

agentctl env list
agentctl env status codex-1
agentctl session list codex-1
agentctl export codex-1 --type dpkg-delta
agentctl export codex-1 --type rootfs-changed-paths
agentctl env stop codex-1
agentctl env destroy codex-1
```

`dpkg-delta` compares package names and versions, reporting installed, removed, and upgraded packages.

`agentctl env create` accepts resource overrides for the `privileged-dev` defaults:

```bash
agentctl env create codex-1 --from base-001 \
  --cpu-max 800% --memory-max 32G --pids-max 8192 --disk-max 200G
```

For `cpu_max`, `memory_max`, `pids_max`, `disk_max`, and `max_runtime`, `0` means unlimited. Unlimited systemd properties are omitted, and unlimited disk does not apply a Btrfs qgroup limit.

The default `network=private-nat` profile launches nspawn with a veth in the `agent-forkd` network zone and writes `/etc/systemd/network/80-agent-forkd-private-nat.network` for the `vz-agent-forkd` bridge. Use `--network private` to request an isolated namespace without egress.

## Metadata Layout

```text
/agentfs
  /bases/<base-id>/manifest.json
  /bases/<base-id>/dpkg.list
  /envs/<env-id>/meta.json
  /envs/<env-id>/sessions/<session-id>.json
  /envs/<env-id>/logs/exec.log
  /envs/<env-id>/logs/sessions/<session-id>.log
  /envs/<env-id>/exports/<export-artifact>
  /runtime/sockets/agent-forkd.sock
```

JSON schemas live in `schemas/`.

`agent-forkd` and `agentctl` accept `--config /etc/agent-forkd/config.json` or `AGENT_FORKD_CONFIG` for the daemon config schema in `schemas/config.schema.json`.

Base freeze creates a writable Btrfs snapshot, removes runtime-only paths such as `/proc`, `/sys`, `/dev`, `/run`, and `/tmp`, scrubs host `/agentfs` state such as `bases`, `envs`, `cache`, and `runtime`, and then marks the base snapshot read-only. Env destroy deletes the child subvolume and explicitly releases the qgroup when Btrfs still exposes it. Export commands print their output and persist the latest artifact under `/agentfs/envs/<env-id>/exports/`.

Env start validates that the child rootfs contains `/bin/bash`, `sudo`, `apt` or `apt-get`, `tmux`, and `tee`. If those tools are missing, the env is marked `failed` and nspawn is not launched.

If nspawn launch fails, the env is marked `failed`. After exec, the daemon checks the Btrfs qgroup and marks the env `quota_exceeded` when the child has reached its disk quota.

Session operations invoke `tmux` through `machinectl shell` inside the child nspawn machine. For interactive attach, `agent-forkd` prepares or resolves the target session and returns the child machine/session to `agentctl`; the CLI then runs `machinectl shell ... tmux attach-session` with the user's terminal attached. The child session command mirrors stdout/stderr through `tee -a` into `/var/log/agent-forkd/sessions/<session-id>.log` inside the child rootfs so pane output stays visible and `/agentfs` does not need to be bind-mounted into the child. `agentctl session logs` pulls that transcript through `machinectl` and writes it to `/agentfs/envs/<env-id>/logs/sessions/<session-id>.log`.

## Security Model

Child environments are not separate VMs. They are privileged development roots inside the Project VM and rely on the outer Kata VM for the kernel boundary. `agent-forkd` still configures nspawn private users and private networking/private NAT, marks `/agentfs` and common Docker socket paths inaccessible, and keeps base and sibling rootfs trees outside the child view.

## Test Notes

Unit tests cover command generation, schema-adjacent metadata behavior, and export delta logic. The ignored Rust integration test in `crates/agentctl/tests/privileged_sequence.rs` covers the full goal sequence and must be run as root inside a privileged Btrfs/systemd-nspawn Project VM. See `tests/environment-requirements.md` for the machine requirements and run the non-destructive preflight first:

```bash
sudo tests/check-privileged-environment.sh
```

Then run:

```bash
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  cargo test -p agentctl --test privileged_sequence -- --ignored --nocapture
```
