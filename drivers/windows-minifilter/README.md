# AgentFs Windows Minifilter

This driver is the opt-in Windows path-preserving overlay backend. The default
Windows desktop backend is driver-free and exposes the env at a separate path;
select this backend explicitly with `--backend windows-minifilter-overlay` when
the command must keep seeing the original host path. `agentctl` creates an env
with lower, upper, and whiteout directories. `agent-minifilterctl` launches the
target process suspended, registers its PID and env roots through the minifilter
communication port, then resumes it.

The driver applies the overlay for registered process trees only:

- read unchanged path: lower
- create/write/truncate path: upper, with lower-to-upper copy-up first
- delete marker present: not found
- delete: remove upper entry if present and create whiteout
- rename: copy source to target upper and whiteout the source
- host path string remains the original source path from user mode

Build from a WDK developer prompt:

```powershell
msbuild drivers\windows-minifilter\agentfs.vcxproj /p:Configuration=Release /p:Platform=x64
```

Install on a test-signed machine:

```powershell
pnputil /add-driver drivers\windows-minifilter\agentfs.inf /install
fltmc load agentfs
agent-minifilterctl check
```

The development smoke script signs the driver package with a local test
certificate. Secure Boot systems must boot with test-signing enabled, or use a
production/attestation signed driver, before `fltmc load agentfs` can succeed.
