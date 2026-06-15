param(
    [string]$Configuration = "Release",
    [string]$Platform = "x64",
    [string]$EnvId = "codex-smoke"
)

$ErrorActionPreference = "Stop"

function Assert-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this smoke test from an elevated Developer PowerShell prompt."
    }
}

function Invoke-Logged {
    param([scriptblock]$Command)
    Write-Host ">> $Command"
    & $Command
}

Assert-Admin

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$agentfs = Join-Path $env:TEMP ("agentfs-minifilter-" + [Guid]::NewGuid().ToString("N"))
$source = Join-Path $env:TEMP ("agentfs-source-" + [Guid]::NewGuid().ToString("N"))
$driverProject = Join-Path $repo "drivers\windows-minifilter\agentfs.vcxproj"
$driverSys = Join-Path $repo "drivers\windows-minifilter\$Platform\$Configuration\agentfs.sys"
$driverInf = Join-Path $repo "drivers\windows-minifilter\agentfs.inf"
$binDir = Join-Path $repo "target\x86_64-pc-windows-msvc\debug"
$agentctl = Join-Path $binDir "agentctl.exe"
$daemon = Join-Path $binDir "agent-forkd.exe"
$filterctl = Join-Path $binDir "agent-minifilterctl.exe"

try {
    New-Item -ItemType Directory -Force -Path $source | Out-Null
    Set-Content -Path (Join-Path $source "host.txt") -Value "host-original"
    Set-Content -Path (Join-Path $source "delete-me.txt") -Value "delete-original"

    Push-Location $repo
    Invoke-Logged { cargo build -p agentctl -p agent-forkd -p agent-minifilterctl --target x86_64-pc-windows-msvc }
    Invoke-Logged { msbuild $driverProject /p:Configuration=$Configuration /p:Platform=$Platform }
    Pop-Location

    if (-not (Test-Path $driverSys)) {
        throw "driver binary was not produced at $driverSys"
    }

    Invoke-Logged { pnputil /add-driver $driverInf /install }
    fltmc unload agentfs 2>$null | Out-Null
    Invoke-Logged { fltmc load agentfs }
    Invoke-Logged { & $filterctl check }

    $env:AGENTFS = $agentfs
    $env:AGENT_MINIFILTERCTL = $filterctl
    Remove-Item Env:\AGENT_WINDOWS_BLOCK_CLONE -ErrorAction SilentlyContinue

    $daemonOut = Join-Path $agentfs "daemon.out.log"
    $daemonErr = Join-Path $agentfs "daemon.err.log"
    New-Item -ItemType Directory -Force -Path $agentfs | Out-Null
    $daemonProcess = Start-Process -FilePath $daemon -ArgumentList @("--agentfs", $agentfs) -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr -PassThru

    $ready = $false
    for ($i = 0; $i -lt 40; $i++) {
        try {
            & $agentctl --agentfs $agentfs init | Out-Null
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $ready) {
        throw "agent-forkd did not become ready"
    }

    & $agentctl --agentfs $agentfs new -t $EnvId --from $source -- powershell.exe -NoProfile -Command @"
`$ErrorActionPreference = 'Stop'
if ((Get-Location).Path -ne '$source') { throw "cwd was not preserved: `$((Get-Location).Path)" }
if ((Get-Content host.txt) -ne 'host-original') { throw 'lower read failed' }
Set-Content host.txt 'env-modified'
Set-Content created.txt 'env-created'
Remove-Item delete-me.txt
Rename-Item created.txt renamed.txt
`$names = Get-ChildItem -Name | Sort-Object
if (`$names -notcontains 'host.txt') { throw 'directory listing lost lower file' }
if (`$names -notcontains 'renamed.txt') { throw 'directory listing lost upper renamed file' }
if (`$names -contains 'delete-me.txt') { throw 'directory listing showed whiteout file' }
"@

    $hostContent = Get-Content (Join-Path $source "host.txt")
    if ($hostContent -ne "host-original") {
        throw "host file was modified: $hostContent"
    }
    if (-not (Test-Path (Join-Path $source "delete-me.txt"))) {
        throw "host delete-me.txt was removed"
    }

    $upperRoot = Join-Path $agentfs "envs\$EnvId\upper"
    $whiteoutRoot = Join-Path $agentfs "envs\$EnvId\whiteouts"
    $relative = $source.Substring(3)
    $upperSource = Join-Path $upperRoot $relative
    $whiteoutSource = Join-Path $whiteoutRoot $relative

    if ((Get-Content (Join-Path $upperSource "host.txt")) -ne "env-modified") {
        throw "modified file was not copied to upper"
    }
    if (-not (Test-Path (Join-Path $upperSource "renamed.txt"))) {
        throw "renamed file was not written to upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "delete-me.txt"))) {
        throw "delete whiteout was not created"
    }

    Write-Host "Windows minifilter smoke passed"
} finally {
    if ($daemonProcess -and -not $daemonProcess.HasExited) {
        Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    }
    fltmc unload agentfs 2>$null | Out-Null
    Remove-Item -Recurse -Force $agentfs, $source -ErrorAction SilentlyContinue
}
