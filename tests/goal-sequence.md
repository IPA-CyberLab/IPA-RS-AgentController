# Privileged Integration Test Sequence

Run inside a Project VM where `/agentfs` is Btrfs and `/` is a Btrfs subvolume.

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
agentctl session attach codex-1 dev

agentctl session create codex-1 codex -- codex
agentctl session list codex-1
agentctl session attach codex-1 codex

agentctl export codex-1 --type dpkg-delta
agentctl export codex-1 --type rootfs-changed-paths

agentctl env stop codex-1
agentctl env destroy codex-1
agentctl env status claude-1
```
