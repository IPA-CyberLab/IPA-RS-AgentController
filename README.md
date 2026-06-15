# IPA-RS Isolated Agent

`agent-forkd` and `agentctl` manage forked privileged development environments inside one Kata-backed Project VM.

The implementation uses:

- Btrfs read-only base snapshots and writable child snapshots
- Btrfs qgroup quotas per child rootfs
- systemd-nspawn machines with `PrivateUsers=yes` and `host`, `bridge`, or `none` networking
- tmux-backed persistent PTY sessions running inside each child machine
- JSON metadata under `/agentfs`
- a Unix socket API at `/agentfs/runtime/sockets/agent-forkd.sock`

## Build

```bash
cargo build --release
sudo install -m 0755 target/release/agent-forkd /usr/local/bin/agent-forkd
sudo install -m 0755 target/release/agentctl /usr/local/bin/agentctl
sudo ln -sf agentctl /usr/local/bin/agctl
sudo install -d -m 0755 /etc/agent-forkd
sudo install -m 0644 packaging/agent-forkd/config.json /etc/agent-forkd/config.json
sudo install -m 0644 packaging/systemd/agent-forkd.service /etc/systemd/system/agent-forkd.service
sudo systemctl daemon-reload
sudo systemctl enable --now agent-forkd
```

For macOS path-preserving native views, install the helper pair through
`install.sh` so `agent-viewd` is root-owned setuid under
`/usr/local/libexec/ipa-rs-isolated-agent` and both helper names are symlinked
from the selected install directory. A plain unprivileged copy of `agent-viewd`
cannot perform the required `chroot`.

## Install

Install the latest GitHub Release binaries and add the install directory to
your shell PATH:

```bash
curl -fsSL https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.sh | sh
```

On Linux, install the binaries to `/usr/local/bin`, install the packaged
systemd service, and restart the daemon:

```bash
curl -fsSL https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.sh |
  AGENT_INSTALL_SERVICE=1 sh
```

On Windows, install the release binaries with PowerShell:

```powershell
iwr https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.ps1 -UseB | iex
```

To also register the native desktop daemon for the current user, set
`AGENT_INSTALL_SERVICE=1`. On macOS this creates a `launchd` LaunchAgent. On
Windows this creates a user Scheduled Task:

```bash
curl -fsSL https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.sh |
  AGENT_INSTALL_SERVICE=1 sh
```

```powershell
$env:AGENT_INSTALL_SERVICE = "1"
iwr https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.ps1 -UseB | iex
```

On Linux, the default agent store is `/agentfs`. On macOS and Windows, the
default is `$HOME/.agentfs`, matching the user-level launchd/Scheduled Task
installed by the service option. That means a native desktop install can be
started with:

```bash
agentctl new -t codex
```

```powershell
agentctl new -t codex
```

For the stable workflow on Windows and macOS, point the client at a Linux host,
WSL VM, or Linux VM where `agent-forkd` is installed:

```powershell
$env:AGENT_REMOTE = "mizuame@100.123.154.79"
agentctl new -t codex
agentctl exec codex -- bash -lc "hostname; test -f /home/mizuame/a.text && echo visible"
```

The same remote mode works on macOS and Linux clients:

```bash
AGENT_REMOTE=mizuame@100.123.154.79 agentctl new -t codex
```

The native desktop daemon path is available for development builds. On Windows
and macOS, `agent-forkd` listens on `tcp_addr` from config, defaulting to
`127.0.0.1:38475`, and uses the desktop backend instead of `systemd-nspawn`:

```powershell
agent-forkd --agentfs "$env:USERPROFILE\.agentfs"
agentctl new -t codex -- cmd /C ver
```

```bash
agent-forkd --agentfs "$HOME/.agentfs"
agentctl new -t codex -- uname -a
```

By default the installer writes `agentctl`, its `agctl` alias, `agent-forkd`,
and available helper binaries such as `agent-viewd` and `agent-overlayfs` to
`$HOME/.local/bin`.
On macOS, `agent-viewd` is installed as a root-owned setuid helper under
`/usr/local/libexec/ipa-rs-isolated-agent` because it must perform mount and
chroot setup before dropping back to the invoking user. The installer leaves an
`agent-viewd` symlink in the selected install directory, and also symlinks the
same-directory `agent-overlayfs` helper for diagnostics and direct helper
checks. The installer fails if the privileged helper is not root-owned setuid,
the helper symlinks are missing, or `agent-overlayfs check` cannot run. When
`AGENT_INSTALL_SERVICE=1` is used on macOS, the LaunchAgent sets
`AGENT_VIEWD` to the privileged helper path so `agent-forkd` does not depend on
the user shell's PATH when starting path-preserving sessions.
Override the release or destination with environment variables:

```bash
AGENT_VERSION=v0.1.0 AGENT_INSTALL_DIR=/usr/local/bin \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/IPA-CyberLab/IPA-RS-IsolatedAgent/master/install.sh)"
```

GitHub Actions builds release archives for Linux, macOS, and Windows on x86_64
and arm64 targets. `agent-forkd` is operational on Linux where the runtime
requirements below are available. Windows and macOS can either use `--remote`
or `AGENT_REMOTE` for the Linux-backed workflow, or run the native desktop
daemon locally. The native backend uses path-preserving overlay metadata and
the `agent-viewd` and `agent-overlayfs` helpers on macOS. On Windows,
`agent-minifilterctl` registers the launched process with the `agentfs`
minifilter so the process keeps seeing the original `C:\Users\...\project`
path while reads resolve through lower/upper/whiteout state and writes are
redirected to the env upper layer. Set `AGENT_WINDOWS_BLOCK_CLONE=1` to use the
older compatibility backend based on `FSCTL_DUPLICATE_EXTENTS_TO_FILE` block
cloning.
macOS path-preserving views require macFUSE. The native backend currently
supports local exec/shell plus `workspace-patch` and `rootfs-changed-paths`
export. macOS exec and shell mount `agent-overlayfs` directly on the selected
source path for the lifetime of the command, so tools see the original absolute
workspace path while writes are copied into the env upper layer and the host
source remains unchanged after unmount. Because macOS has no per-process mount
namespace, the installer adds a small `agctl` shell wrapper that temporarily
moves the calling shell out of the workspace and passes the original directory
through `AGENT_HOST_CWD`; this keeps mount/unmount from invalidating the
terminal's current directory. The command itself runs with `HOME`, `ZDOTDIR`,
and temp variables pointed at the overlaid source path. macOS path-preserving
views support `network=host` and `network=none`; `bridge` is a Linux nspawn mode
and is rejected instead of silently running with host networking. `network=none`
wraps the entered command in the macOS sandbox profile that denies `network*`.
Windows minifilter execution starts the command suspended, registers its PID and
overlay roots through the filter communication port, assigns it to a Job
Object, then resumes it. The compatibility block-clone backend is still weaker
than the Linux `systemd-nspawn` backend: it runs inside a Job Object rooted at
the env directory, and native desktop sessions support background
create/list/logs/kill but not interactive attach yet.

On a Windows development machine with WDK installed and test-signing enabled,
build, load, and verify the minifilter path-preserving backend from an elevated
Developer PowerShell prompt:

```powershell
scripts\windows-minifilter-smoke.ps1
```

The smoke test builds `agentctl`, `agent-forkd`, `agent-minifilterctl`, and the
`agentfs` minifilter, loads the filter, runs a command from the original host
project path, and verifies that host files stay unchanged while modified,
renamed, and deleted entries appear under the env upper/whiteout trees.

After installing on a macOS host with macFUSE, run the native backend smoke
test to verify the privileged helper and runtime path view end to end:

```bash
scripts/macos-native-smoke.sh
```

macFUSE must be installed and its kernel extension must be approved and loaded
before the smoke test can mount a view. A successful setup exposes a FUSE device
such as `/dev/fuse`, `/dev/macfuse0`, or `/dev/osxfuse0`. If macFUSE has just
been installed, approve it in macOS System Settings -> Privacy & Security, then
rerun the smoke test. On a host where approval is already possible, these
commands are useful diagnostics:

```bash
scripts/macos-macfuse-preflight.sh
```

GitHub-hosted macOS runners can build the helper binaries and verify the
installer, but they cannot approve third-party kernel extensions interactively.
For that reason the full native macOS smoke test must run on a real macOS host
or a self-hosted runner where macFUSE is pre-approved and the FUSE device is
available. The repository includes a manual `macOS native smoke` workflow for
that purpose; attach a self-hosted runner with the `self-hosted` and `macOS`
labels, approve/load macFUSE on that host, then dispatch the workflow from
GitHub Actions.

The smoke test requires the installed `agent-viewd` to resolve to a root-owned
setuid helper, verifies that a macFUSE device is available, checks that
`agent-overlayfs` is callable, starts `agent-forkd`, verifies `/bin/zsh`,
`/usr/bin/env`, `/System`, preserved cwd, and confirms that broad host fallback
siblings like `/private/var/db`, `/usr/local`, and `/Library/Application
Support` are not visible. It also checks that the broad `/private/etc` config
tree is not visible and that `network=none` cannot reach a local TCP listener
while `network=host` can.

## Requirements

The Project VM must provide Linux, Btrfs, `btrfs-progs`, systemd, `systemd-nspawn`, `machinectl`, `systemd-networkd`, cgroup v2, user namespaces, `tmux`, and `tee`. The full privileged goal sequence also expects Debian/Ubuntu package tooling (`apt` or `apt-get`, `dpkg`, and `sudo`) and the `codex` CLI to be available in the host rootfs before freezing a base.

`/agentfs` must be on a Btrfs filesystem. If the requested source root is a
Btrfs subvolume, environments use Btrfs snapshots. Otherwise, Linux hosts can
fall back to OverlayFS copy-on-write rootfs mounts.

Base, env, session, and profile IDs are hostname-safe identifiers: ASCII
letters and numbers with optional interior `-`, such as `base-001` or
`codex-1`.

## Usage

```bash
agentctl new -t codex
agentctl new -t codex -- echo ready

agentctl init --agentfs /agentfs
agentctl base freeze --name base-001 --from /

agentctl env create codex-1 --from base-001 --profile privileged-dev
agentctl env create claude-1 --from base-001 --profile privileged-dev
agentctl env start codex-1
agentctl env start claude-1

agentctl exec codex-1 -- sudo apt update
agentctl exec codex-1 -- sudo apt install -y ripgrep
agentctl shell codex-1
agentctl session create codex-1 dev -- bash
agentctl session attach codex-1 dev
agentctl session detach codex-1 dev
agentctl session logs codex-1 dev

agentctl env list
agentctl env status codex-1
agentctl session list codex-1
agentctl diff codex-1
agentctl apply codex-1
agentctl export codex-1 --type workspace-patch
agentctl export codex-1 --type dpkg-delta
agentctl export codex-1 --type rootfs-changed-paths
agentctl env stop codex-1
agentctl env destroy codex-1
```

`agentctl new -t <env-id>` is the tmux-style entrypoint. It initializes
`/agentfs`, creates `base-001` from `/` when that base does not exist, creates
the target env when needed, starts it, and attaches the persistent `shell`
session. Supplying a command after `--` performs the same bootstrap and then
executes that command instead of attaching a shell.

On native macOS and Windows, `agentctl new -t <env-id>` uses `$HOME/.agentfs`
and clones the current directory by default. Pass `--from <path>` to choose a
different native source tree.

`agentctl shell <env-id>` creates or reuses a persistent `shell` tmux session
inside the child and attaches the current terminal to it. `agentctl diff`
prints the `/workspace` Git patch when that directory is a Git repository, and
`workspace-patch` also persists the patch artifact under the env's `exports`
directory.

On macOS path-preserving overlay envs, `agentctl apply <env-id>` applies the
env's upper layer and whiteouts back to the original source tree. It refuses to
overwrite host paths that changed since env creation unless `--force` is passed.

`dpkg-delta` compares package names and versions, reporting installed, removed, and upgraded packages.

`agentctl env create` uses `default_profile` from the daemon config when `--profile` is omitted. The packaged config sets that default to `privileged-dev`. Resource overrides can be supplied on the CLI:

```bash
agentctl env create codex-1 --from base-001 \
  --cpu-max 800% --memory-max 32G --pids-max 8192 --disk-max 200G
```

For `cpu_max`, `memory_max`, `pids_max`, `disk_max`, `idle_timeout`, and `max_runtime`, `0` means unlimited. Unlimited systemd properties are omitted, and unlimited disk does not apply a Btrfs qgroup limit. Nonzero `idle_timeout` values are checked during status/list refresh and stop a running env after the recorded `last_active_at` age exceeds the limit.

The default `network=host` profile uses host networking. Use `--network bridge` on Linux for the nspawn veth/NAT bridge, which writes `/etc/systemd/network/80-agent-forkd-bridge.network` for the `vz-agent-forkd` bridge. Child DNS uses `ResolvConf=copy-host` / `--resolv-conf=copy-host` so apt, GitHub, and API egress can resolve names through the Project VM resolver. Use `--network none` to request an isolated namespace without egress.
Profiles also accept an optional `network_policy` block with `egress_proxy` and `allowlist` fields so proxy or allowlist enforcement can be added without changing the profile schema.

## Metadata Layout

```text
/agentfs
  /bases/<base-id>/manifest.json
  /bases/<base-id>/dpkg.list
  /envs/<env-id>/meta.json
  /envs/<env-id>/sessions/<session-id>.json
  /envs/<env-id>/logs/agent-forkd.log
  /envs/<env-id>/logs/lifecycle.log
  /envs/<env-id>/logs/exec.log
  /envs/<env-id>/logs/nspawn.log
  /envs/<env-id>/logs/sessions/<session-id>.log
  /envs/<env-id>/exports/<export-artifact>
  /runtime/sockets/agent-forkd.sock
```

JSON schemas for daemon config and metadata live in `schemas/`.

`agent-forkd` and `agentctl` accept `--config /etc/agent-forkd/config.json` or `AGENT_FORKD_CONFIG` for the daemon config schema in `schemas/config.schema.json`. Base, env, and session metadata are described by `schemas/base.schema.json`, `schemas/env.schema.json`, and `schemas/session.schema.json`.

Base freeze creates a writable Btrfs snapshot, removes runtime-only paths such as `/proc`, `/sys`, `/dev`, `/run`, and `/tmp`, scrubs host `/agentfs` state, and then marks the base snapshot read-only. Env destroy deletes the child subvolume and explicitly releases the qgroup when Btrfs still exposes it. Export commands print their output and persist the latest artifact under `/agentfs/envs/<env-id>/exports/`. The `rootfs-changed-paths` export omits runtime-only trees such as `/proc`, `/sys`, `/dev`, `/run`, and `/tmp`.
When freezing from `/`, base metadata records `source` as `current-project-vm`.

Env start validates that the child rootfs contains `/bin/bash`, `sudo`, `apt` or `apt-get`, `tmux`, and `tee`. If those tools are missing, the env is marked `failed` and nspawn is not launched.

If nspawn launch fails, the env is marked `failed`. After exec, the daemon checks the Btrfs qgroup and marks the env `quota_exceeded` when the child has reached its disk quota. Env activity updates `last_active_at` for exec, session, shell, diff, and export requests.

Session operations invoke `tmux` through `machinectl shell` inside the child nspawn machine. For interactive attach, `agent-forkd` prepares or resolves the target session and returns the child machine/session to `agentctl`; the CLI then runs `machinectl shell ... tmux attach-session` with the user's terminal attached. The child session command mirrors stdout/stderr through `tee -a` into `/var/log/agent-forkd/sessions/<session-id>.log` inside the child rootfs so pane output stays visible and `/agentfs` does not need to be bind-mounted into the child. `agentctl session logs` pulls that transcript through `machinectl` and writes it to `/agentfs/envs/<env-id>/logs/sessions/<session-id>.log`.

## Security Model

Child environments are not separate VMs. They are privileged development roots inside the Project VM and rely on the outer Kata VM for the kernel boundary. `agent-forkd` still configures nspawn private users, applies the selected network mode, marks `/agentfs` and common Docker socket paths inaccessible, and keeps base and sibling rootfs trees outside the child view.

On macOS and Windows, the native desktop backend is copy-on-write workspace
isolation, not a VM boundary. On macOS, new native environments store a lower
snapshot, upper layer, whiteouts, and hidden `view-root`; commands enter that
view through `agent-viewd` so the visible cwd can remain the original
`/Users/...` path while writes are directed to the env upper layer. `agent-viewd`
is a privileged helper, `agent-overlayfs` performs the macFUSE mount, and
`agent-viewd` only accepts the fixed path-preserving env layout
`<agentfs>/envs/<id>/{lower,upper,whiteouts,view-root}` without `..` or symlink
components before it creates or mounts privileged paths. Neither helper must
fall back to running directly in the host workspace. Windows minifilter envs
use the same lower/upper/whiteout metadata but enforce the view in kernel mode
for registered PIDs only. The driver is a filesystem isolation mechanism, not a
security boundary equivalent to Linux namespaces, and it requires normal Windows
driver signing/test-signing discipline. Native desktop sessions are tracked as
background host processes with transcript files under the env's session log
directory; macOS and Windows minifilter session commands use the same
path-preserving view as exec.

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
