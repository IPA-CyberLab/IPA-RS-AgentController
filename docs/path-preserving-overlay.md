# Path-Preserving Overlay Backend

This document defines the target design for a macOS backend that preserves host
absolute paths while keeping all mutations isolated inside an agent environment.

## Goal

The user should be able to run:

```text
cd /Users/mizuame/Desktop/script/example
agctl shell codex
```

and see the same path inside the environment:

```text
/Users/mizuame/Desktop/script/example
```

Reads should see the environment view of that path. Writes, deletes, renames,
and editor safe-save flows must be recorded in the environment only. The host
filesystem must not be mutated by commands running inside the environment.

This is not the same as the current native desktop backend. The current backend
enters an APFS-cloned rootfs path and uses `sandbox-exec` to limit writes. This
backend instead provides a virtual filesystem namespace whose paths match the
host namespace.

## User-Facing Semantics

- `pwd` preserves the original absolute path when entering the environment.
- `realpath .` returns the preserved absolute path inside the environment.
- Opening an unchanged file reads the lower snapshot.
- Creating or modifying a file writes to the environment upper layer.
- Deleting a lower file creates a whiteout/tombstone in the environment.
- Renaming across unchanged lower files performs copy-up plus whiteout.
- Editor safe-save sequences, such as write temp file then rename over target,
  are treated as environment mutations only.
- Host files are never modified by environment processes.
- Multiple environments may expose the same absolute paths but keep independent
  upper layers.
- Export and diff operate against the lower snapshot plus upper layer.

## Lower Layer Policy

The default lower layer is a snapshot captured when the environment is created.
That makes the environment reproducible and avoids host edits racing with the
environment view.

An optional future `live-lower` mode may read through to the host for files that
have not been copied up. That mode is useful for inspecting host changes but is
not the default because it weakens reproducibility.

## Namespace Model

The backend creates an environment view root:

```text
~/.agentfs-ipa-rs-current/envs/<env-id>/view-root
```

That view root is not shown to the user. It is used as the process root for a
chrooted process tree. Inside the chroot, the path layout mirrors macOS:

```text
/Users/mizuame/Desktop/script/example
/System
/Library
/usr
/bin
/opt/homebrew
/private/tmp
/dev/null
/dev/tty
```

The important property is that `/Users/mizuame/Desktop/script/example` inside
the chroot is not the host directory. It is a virtual overlay entry with the
same absolute path string.

## Filesystem Layout

Each environment stores:

```text
envs/<env-id>/
  lower/        snapshot metadata and cloned lower roots
  upper/        file contents created or modified by the env
  whiteouts/    deletion markers for lower files
  view-root/    chroot root containing mounted virtual views
  meta.json
```

The overlay resolver handles paths in this order:

1. Whiteout exists: report not found.
2. Upper entry exists: return upper.
3. Lower snapshot entry exists: return lower.
4. Otherwise: report not found.

## Runtime Architecture

The backend requires a privileged helper because normal macOS user processes
cannot create a private mount namespace or enter chroot.

Components:

- `agent-forkd`: control plane, metadata, env lifecycle.
- `agent-viewd`: privileged macOS helper for mount/chroot setup.
- `agent-overlayfs`: filesystem implementation using FSKit or macFUSE.
- `agentctl`: client that requests shells and commands.

Launch flow:

1. `agent-forkd` resolves env metadata and target cwd.
2. `agent-viewd` ensures the overlay view is mounted.
3. `agent-viewd` enters `chroot(view-root)`.
4. `agent-viewd` drops privileges back to the invoking user.
5. The shell or command starts with cwd preserved inside the chroot.
6. `sandbox-exec` or an equivalent policy still constrains network and process
   behavior where possible.

## Overlay Requirements

The filesystem layer must support:

- POSIX open/read/write/truncate/ftruncate/fsync.
- `rename` and atomic replace semantics used by editors.
- Directory create/delete/list.
- Symlinks, including symlink targets resolved inside the chroot.
- File metadata required by developer tooling: mode, mtime, size, xattrs where
  feasible.
- File locks sufficiently for package managers and editors.
- `mmap` behavior for compilers and language servers.
- Tombstones for lower deletions.
- Stable inode identity within one environment session where practical.

Hard links may be degraded initially by copy-up unless a tool requires exact
link count behavior.

## Security Model

`chroot` is a namespace mechanism, not a complete security boundary. The
backend must:

- Run the chrooted process tree as the normal user, not root.
- Keep privileged setup in a small helper.
- Avoid granting host-write paths to the sandboxed process.
- Treat the overlay upper layer as the only write target.
- Continue to honor the selected network mode.
- Deny or virtualize writes to system paths unless explicitly configured.

The backend should not rely on `chroot` alone if commands can gain root inside
the environment.

## macOS Implementation Options

### FSKit

FSKit is Apple's filesystem-extension framework for implementing filesystems on
macOS. It is the preferred long-term implementation path for a first-class
native backend, but it requires an FSKit module and the relevant Apple
entitlement/signing flow.

### macFUSE

macFUSE is a practical development and compatibility path. It can validate the
overlay semantics before committing to FSKit. It does require a user-approved
system extension or kernel-extension style installation depending on the macOS
version and macFUSE release.

### Endpoint Security

Endpoint Security is not the primary filesystem view mechanism. It can monitor
or authorize file events, but it does not provide path-preserving copy-on-write
filesystem semantics by itself.

### File Provider

File Provider is a sync-provider model, not a transparent process-local overlay
filesystem for arbitrary CLI tools. It is not the right primitive for this
backend.

## Non-Goals

- No global replacement of `/Users/...` for the whole host.
- No mutation of the original host tree during environment execution.
- No Finder-level illusion as the primary interface. The first target is CLI
  processes launched through `agentctl`.
- No best-effort syscall interposition as the core correctness mechanism.

## Open Decisions

- FSKit first versus macFUSE prototype first.
- Whether lower defaults to APFS cloned snapshot or live host passthrough.
- How much of `/` should be exposed in the chroot by default.
- How to model writes outside configured workspaces.
- Whether `agctl apply <env>` should merge upper changes back into the host.
- How to package and approve the privileged helper and filesystem extension.

## References

- Apple FSKit documentation: https://developer.apple.com/documentation/FSKit
- Apple Endpoint Security documentation: https://developer.apple.com/documentation/EndpointSecurity
- Apple File Provider documentation: https://developer.apple.com/documentation/fileprovider
