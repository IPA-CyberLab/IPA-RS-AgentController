# AgentFs Windows Minifilter

This driver is the Windows path-preserving overlay backend. `agentctl` creates
an env with lower, upper, and whiteout directories. `agent-minifilterctl`
launches the target process suspended, registers its PID and env roots through
the minifilter communication port, then resumes it.

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
