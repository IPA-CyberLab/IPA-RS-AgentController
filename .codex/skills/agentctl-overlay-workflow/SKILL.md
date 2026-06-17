---
name: agentctl-overlay-workflow
description: >-
  Use agentctl/agctl native isolated development environments for Codex or Claude Code on macOS or Windows, including path-preserving overlay behavior, Windows default clone backend behavior, optional Windows minifilter backend handling, launching codex or claude inside envs, checking env state, diff/export inspection, applying overlay changes back to the host with agctl apply, handling conflicts, rebuilding/installing helper binaries, and avoiding accidental host writes. Use when the user mentions agctl, agentctl, IPA-RS-IsolatedAgent, path-preserving overlay, view-root, env upper/whiteouts/lower, syncing/applying env changes to host, running Codex or Claude Code inside an agentctl environment, Windows minifilter, or confusion about whether changes are inside or outside the env.
---

# Agentctl Overlay Workflow

Use this skill when operating this repository's `agentctl` / `agctl` workflow on macOS or Windows.

## Mental Model

- `agctl new -t <env>` creates or enters an env whose working tree is an overlay view.
- The env prompt may show `codex@...` or another env id. Confirm with `pwd`.
- For path-preserving overlay envs, the real cwd is usually `~/.agentfs/envs/<env>/view-root`.
- Relative writes inside that cwd go to the env overlay, not directly to the host source tree.
- Host `HOME` is shared intentionally so Codex, Claude Code, MCP, auth, Keychain, and app databases work normally.
- Process sandboxing is intentionally disabled for this backend. Do not reintroduce `CODEX_HOME` hacks or `sandbox-exec` workarounds unless the user explicitly asks.
- Because there is no process sandbox, absolute writes to the original host source path can bypass the overlay. Keep repo edits inside the env cwd/view-root unless the user explicitly wants direct host edits.

## Backend Selection

- macOS default native desktop backend is `apfs-clone`.
- Windows default native desktop backend is `windows-block-clone`; it does not preserve the original host path. The env command runs under the cloned env rootfs, and host source files should remain unchanged.
- Windows `windows-block-clone` may fall back to ordinary copying when `FSCTL_DUPLICATE_EXTENTS_TO_FILE` is unsupported by the filesystem or policy.
- Path-preserving Windows minifilter behavior is opt-in only:

```sh
agentctl new -t codex --backend windows-minifilter-overlay --from <project>
```

- Do not switch Windows users to `windows-minifilter-overlay` unless they explicitly ask for the kernel-driver backend.
- The minifilter backend requires a signed/loadable kernel driver. Test-signed drivers are blocked when Secure Boot is enabled and test signing is off; production or attestation signing is needed for that environment.

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

1. Start or enter an env:

```sh
agctl new -t codex
```

2. Launch the agent inside it when needed:

```sh
codex
claude
```

3. Verify location when behavior is confusing:

```sh
pwd
agctl ls
```

4. Prefer env-local relative paths. Avoid editing the original host source absolute path from inside the env.

## Inspecting Changes

For path-preserving overlay envs, prefer changed-path export:

```sh
agctl export codex --type rootfs-changed-paths
```

`agctl diff` is mostly useful for the older `/workspace` Git-patch model. It may be empty for path-preserving overlay envs.

## Applying Changes Back To Host

Use apply only after the user wants env changes reflected in the host tree:

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

After code changes to CLI behavior:

```sh
cargo test --workspace
cargo build --release -p agentctl
install -m 755 target/release/agentctl "$HOME/.local/bin/agentctl"
```

After code changes to the privileged macOS view helper:

```sh
cargo build --release -p agent-viewd
sudo install -o root -g wheel -m 4755 target/release/agent-viewd /usr/local/libexec/ipa-rs-isolated-agent/agent-viewd
```

After helper changes, ask the user to exit old env shells and recreate/reenter the env:

```sh
exit
agctl rm codex
agctl new -t codex
```

On Windows, local builds or downloaded CI artifacts may be blocked by Windows Defender Application Control/App Control if the binaries are unsigned. If the user has provided a real Windows test machine and wants execution there, prefer CI-built artifacts, then verify `agentctl.exe` and `agent-forkd.exe` can run. A local trusted code-signing certificate can make user-mode test binaries executable on that machine, but it does not solve kernel driver loading under Secure Boot.

## Troubleshooting

- If Codex or Claude Code auth/MCP/network/Keychain fails inside env, first confirm the installed helper is current. The backend should not block those services.
- If `codex` says local DB cannot open under `view-root`, suspect stale helper or old HOME behavior.
- If terminal/PTY operations fail with `Operation not permitted`, suspect stale helper from the old sandboxed implementation.
- If `agctl apply` reports a host conflict, inspect the host file and env changed path before using `--force`.
- If files appear in env but not host, that is expected until `agctl apply <env>` is run.
- On Windows default backend, seeing an env cwd under `.agentfs\envs\<env>\rootfs` is expected; same-path visibility requires the opt-in minifilter backend.
- On Windows, `FSCTL_DUPLICATE_EXTENTS_TO_FILE failed` means block clone is unavailable for that source/target; use a build with copy fallback or expect env creation to fail.
- For Windows minifilter failures, check `Confirm-SecureBootUEFI`, `bcdedit /enum {current}`, and `fltmc load agentfs` output before debugging overlay semantics.
