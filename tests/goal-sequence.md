# Privileged Integration Test Sequence

Run inside a Project VM where `/agentfs` is Btrfs and `/` is a Btrfs
subvolume. See `tests/environment-requirements.md` for the full machine
requirements. Before running the destructive sequence, install and start
`agent-forkd`, then run the non-destructive preflight:

```bash
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  tests/check-privileged-environment.sh
```

The ignored Rust integration test automates the goal sequence without opening
an interactive tmux attach:

```bash
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  cargo test -p agentctl --test privileged_sequence -- --ignored --nocapture
```

The manual sequence below is useful when validating the interactive attach and
reattach behavior by hand.

```bash
sudo systemctl start agent-forkd
agentctl init --agentfs /agentfs
agentctl base freeze --name base-001 --from /

agentctl env create codex-1 --from base-001 --profile privileged-dev
agentctl env create claude-1 --from base-001 --profile privileged-dev
agentctl env start codex-1
agentctl env start claude-1

agentctl exec codex-1 -- sudo apt update
agentctl exec codex-1 -- sudo apt install -y ripgrep
agentctl exec codex-1 -- rg --version
agentctl exec claude-1 -- bash -lc 'command -v rg || true'

agentctl exec codex-1 -- bash -lc 'echo codex > /root/marker.txt'
agentctl exec claude-1 -- bash -lc 'test ! -e /root/marker.txt'

agentctl session create codex-1 dev -- bash
agentctl session attach codex-1 dev
# Detach with the tmux default key sequence: Ctrl-b, then d.
agentctl session attach codex-1 dev
agentctl session detach codex-1 dev
agentctl session logs codex-1 dev

agentctl session create codex-1 codex -- codex
agentctl session list codex-1
agentctl session attach codex-1 codex
# Detach with Ctrl-b, then d.
agentctl session detach codex-1 codex
agentctl session logs codex-1 codex

agentctl export codex-1 --type dpkg-delta
agentctl export codex-1 --type rootfs-changed-paths

agentctl env stop codex-1
agentctl env destroy codex-1
agentctl env status claude-1
```

After the manual sequence, remove the remaining sibling env if the machine will
be reused:

```bash
agentctl env destroy claude-1 || true
sudo machinectl terminate af-claude-1 || true
sudo rm -rf /agentfs/bases/base-001 /agentfs/envs/claude-1
```
