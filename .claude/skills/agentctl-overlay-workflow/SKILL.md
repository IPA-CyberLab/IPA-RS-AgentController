---
name: agentctl-overlay-workflow
description: >-
  Use agentctl/agctl native isolated development environments for Codex or Claude Code on macOS or Windows, including the default driverless clone backends, optional path-preserving overlay backends, launching codex or claude inside envs, checking env state, inspecting diffs/exports, applying path-preserving overlay changes back to the host with agctl apply, handling conflicts, rebuilding/installing helper binaries, and avoiding accidental host writes. Use when the user mentions agctl, agentctl, IPA-RS-IsolatedAgent, apfs-clone, windows-block-clone, path-preserving overlay, macFUSE, Windows minifilter, view-root, env upper/whiteouts/lower, syncing/applying env changes to host, running Codex or Claude Code inside an agentctl environment, or confusion about whether changes are inside or outside the env.
---

# Agentctl Overlay Workflow

Use this skill when operating this repository's `agentctl` / `agctl` workflow on macOS or Windows.

## Mental Model

- `agctl new -t <env>` creates or enters an isolated native desktop env.
- With no `--backend`, macOS uses `apfs-clone` and Windows uses `windows-block-clone`; these are driverless cloned workspaces, not live overlays.
- Default clone envs run from the env root path, usually `~/.agentfs/envs/<env>/rootfs`. Host source files remain unchanged, but host changes made after env creation are not live-reflected into the env.
- `--backend path-preserving-overlay` on macOS and `--backend windows-minifilter-overlay` on Windows are opt-in path-preserving backends.
- Path-preserving envs use `lower`, `upper`, `whiteouts`, and `view-root`. Relative writes inside the env view go to the env overlay, not directly to the host source tree.
- Existing envs and bases keep their original backend. Remove the env or use a different target/base when switching between default clone and path-preserving modes.
- On macOS, there is no per-process mount namespace. Do not mount over the original host project path as a workaround: host processes will see that global mount. The private `view-root` mode is the meaningful isolated macOS behavior; prompt or `PWD` may be host-like, but syscall-level cwd is not guaranteed to be the original host path.
- On Windows, syscall-level same-path isolation requires the opt-in minifilter backend.
- Host `HOME` handling differs by backend: default macOS clone shells set `HOME`/`ZDOTDIR` inside the env root, while macOS path-preserving direct execution uses the host home so Codex, Claude Code, MCP, auth, Keychain, and app databases keep working.
- Process sandboxing differs by backend. Do not assume a separate VM boundary on native desktop backends. Absolute writes to the original host source path can bypass the env unless the backend/OS specifically intercepts them.

## Backend Selection

- Default macOS command:

```sh
agctl new -t codex
```

Uses `apfs-clone`. It requires no macFUSE, kernel extension, or driver.

- Default Windows command:

```sh
agctl new -t codex
```

Uses `windows-block-clone`. It requires no minifilter driver. It may fall back to ordinary copying when block clone is unsupported by the filesystem or policy.

- Opt-in macOS path-preserving overlay:

```sh
agctl new -t codex --backend path-preserving-overlay --from <project>
```

Requires macFUSE/helper support. Use it only when the user explicitly wants overlay semantics and accepts the macOS path limitations.

- Opt-in Windows path-preserving minifilter:

```sh
agctl new -t codex --backend windows-minifilter-overlay --from <project>
```

Requires a signed/loadable kernel driver. Test-signed drivers are blocked when Secure Boot is enabled and test signing is off; production or attestation signing is needed for that environment.

## Common Commands

```sh
agctl ls
agctl new -t codex
agctl new -t claude
agctl shell codex
agctl exec codex -- <command>...
agctl export codex --type rootfs-changed-paths
agctl apply codex
agctl apply codex --force
agctl rm codex
```

Use `agentctl` and `agctl` interchangeably; `agctl` is the installed alias.

## Working Inside Env

1. Start or enter a default driverless env:

```sh
agctl new -t codex
```

2. Start an opt-in path-preserving env only when needed:

```sh
agctl new -t codex --backend path-preserving-overlay --from "$PWD"
```

3. Launch the agent inside it when needed:

```sh
codex
claude
```

4. Verify location and backend when behavior is confusing:

```sh
pwd
agctl ls
cat ~/.agentfs/envs/<env>/meta.json
```

5. Prefer env-local relative paths. Avoid editing the original host source absolute path from inside the env unless the user explicitly wants direct host edits.

## Inspecting Changes

Use changed-path export to see env-side changed paths:

```sh
agctl export codex --type rootfs-changed-paths
```

`agctl diff` is mostly useful for the older `/workspace` Git-patch model and may be empty for native desktop workflows.

## Applying Changes Back To Host

`agctl apply` is implemented for native desktop path-preserving overlay envs, not the default clone backends.

Use apply only after the user wants path-preserving env changes reflected in the host tree:

```sh
agctl apply codex
```

Behavior:

- Copies env `upper` changes to the original host source tree.
- Applies `whiteouts` as host-side deletions.
- Updates env `lower` and clears applied `upper`/`whiteout` entries so the env becomes clean for those paths.
- Refuses if the host path changed since env creation.

Use `--force` only when the user explicitly accepts overwriting host-side changes:

```sh
agctl apply codex --force
```

Never run `agctl rm <env>` before applying if the user wants to keep env edits.

## Rebuilding And Installing

After code changes to CLI or daemon behavior:

```sh
cargo test --workspace
cargo build --release -p agentctl -p agent-forkd
install -m 755 target/release/agentctl "$HOME/.local/bin/agentctl"
install -m 755 target/release/agent-forkd "$HOME/.local/bin/agent-forkd"
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.ipa-cyberlab.agent-forkd.plist" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.ipa-cyberlab.agent-forkd.plist"
```

After code changes to macOS path-preserving helpers:

```sh
cargo test -p agent-viewd -p agent-overlayfs
cargo build --release -p agent-viewd -p agent-overlayfs
install -m 755 target/release/agent-viewd "$HOME/.local/bin/agent-viewd"
install -m 755 target/release/agent-overlayfs "$HOME/.local/bin/agent-overlayfs"
```

If the installed LaunchAgent points `AGENT_VIEWD` at `/usr/local/libexec/ipa-rs-isolated-agent/agent-viewd`, either update the LaunchAgent to the user helper or install the privileged helper with sudo:

```sh
sudo install -o root -g wheel -m 755 target/release/agent-overlayfs /usr/local/libexec/ipa-rs-isolated-agent/agent-overlayfs
sudo install -o root -g wheel -m 4755 target/release/agent-viewd /usr/local/libexec/ipa-rs-isolated-agent/agent-viewd
```

After helper changes, ask the user to exit old env shells and recreate/reenter the env:

```sh
exit
agctl rm codex
agctl new -t codex
```

On Windows, local builds or downloaded CI artifacts may be blocked by Windows Defender Application Control/App Control if the binaries are unsigned. Prefer CI-built artifacts for real Windows smoke tests. A local trusted code-signing certificate can make user-mode test binaries executable on that machine, but it does not solve kernel driver loading under Secure Boot.

## Troubleshooting

- If a default macOS or Windows env runs under `~/.agentfs/envs/<env>/rootfs`, that is expected.
- If a macOS path-preserving env shows `agent-overlayfs on <original project path>` in `mount`, it is using a legacy/global source mount; exit the env, unmount if necessary, and recreate with the current private `view-root` behavior.
- If Codex or Claude Code auth/MCP/network/Keychain fails inside a path-preserving env, first confirm the installed helper and `AGENT_VIEWD` are current.
- If `codex` says local DB cannot open under `view-root`, suspect stale helper or old HOME behavior.
- If terminal/PTY operations fail with `Operation not permitted`, suspect stale helper from the old sandboxed implementation.
- If `agctl apply` reports a host conflict, inspect the host file and env changed path before using `--force`.
- If files appear in a path-preserving env but not host, that is expected until `agctl apply <env>` is run.
- On Windows default backend, same-path visibility requires the opt-in minifilter backend.
- On Windows, `FSCTL_DUPLICATE_EXTENTS_TO_FILE failed` means block clone is unavailable for that source/target; use a build with copy fallback or expect env creation to fail.
- For Windows minifilter failures, check `Confirm-SecureBootUEFI`, `bcdedit /enum {current}`, and `fltmc load agentfs` output before debugging overlay semantics.
