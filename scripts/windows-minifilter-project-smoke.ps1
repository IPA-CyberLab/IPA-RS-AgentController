param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot "..")),
    [string]$EnvId = ("codex-project-smoke-" + [Guid]::NewGuid().ToString("N").Substring(0, 8)),
    [string]$AgentFs = (Join-Path $env:TEMP ("agentfs-project-smoke-" + [Guid]::NewGuid().ToString("N")))
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
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

function ConvertTo-AgentFsRelativePath {
    param([string]$Path)
    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($full)
    if (-not $full.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
        throw "could not compute overlay relative path for $Path"
    }
    return $full.Substring($root.Length).TrimStart('\', '/')
}

function Start-AgentFsFilter {
    Write-Host ">> fltmc load agentfs"
    $output = & fltmc load agentfs 2>&1
    $exitCode = $LASTEXITCODE
    if ($output) {
        $output | ForEach-Object { Write-Host $_ }
    }
    if ($exitCode -ne 0) {
        Write-Host "fltmc load agentfs returned $exitCode; checking whether the filter is already available"
    }
}

Assert-Admin

$Repo = (Resolve-Path $Repo).Path
$binDir = Join-Path $Repo "target\x86_64-pc-windows-msvc\debug"
$agentctl = Join-Path $binDir "agentctl.exe"
$daemon = Join-Path $binDir "agent-forkd.exe"
$filterctl = Join-Path $binDir "agent-minifilterctl.exe"

if (-not (Test-Path $agentctl) -or -not (Test-Path $daemon) -or -not (Test-Path $filterctl)) {
    Push-Location $Repo
    Invoke-Logged { cargo build -p agentctl -p agent-forkd -p agent-minifilterctl --target x86_64-pc-windows-msvc }
    Pop-Location
}

if (-not (Test-Path $filterctl)) {
    throw "agent-minifilterctl.exe was not produced at $filterctl"
}

Start-AgentFsFilter
Invoke-Logged { & $filterctl check }

$runId = [Guid]::NewGuid().ToString("N")
$readName = ".agentfs-project-read-$runId.txt"
$writeName = ".agentfs-project-write-$runId.txt"
$deleteName = ".agentfs-project-delete-$runId.txt"
$createName = ".agentfs-project-created-$runId.txt"

$readPath = Join-Path $Repo $readName
$writePath = Join-Path $Repo $writeName
$deletePath = Join-Path $Repo $deleteName
$createPath = Join-Path $Repo $createName
$daemonProcess = $null

try {
    New-Item -ItemType Directory -Force -Path $AgentFs | Out-Null
    Set-Content -Path $readPath -Value "host-read-original"
    Set-Content -Path $writePath -Value "host-write-original"
    Set-Content -Path $deletePath -Value "host-delete-original"
    Remove-Item -Force $createPath -ErrorAction SilentlyContinue

    $env:AGENTFS = $AgentFs
    $env:AGENT_MINIFILTERCTL = $filterctl
    $env:AGENT_PROJECT_SMOKE_REPO = $Repo
    $env:AGENT_PROJECT_SMOKE_READ = $readName
    $env:AGENT_PROJECT_SMOKE_WRITE = $writeName
    $env:AGENT_PROJECT_SMOKE_DELETE = $deleteName
    $env:AGENT_PROJECT_SMOKE_CREATE = $createName
    Remove-Item Env:\AGENT_WINDOWS_BLOCK_CLONE -ErrorAction SilentlyContinue

    $daemonOut = Join-Path $AgentFs "daemon.out.log"
    $daemonErr = Join-Path $AgentFs "daemon.err.log"
    $daemonProcess = Start-Process -FilePath $daemon -ArgumentList @("--agentfs", $AgentFs) -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr -PassThru

    $ready = $false
    for ($i = 0; $i -lt 40; $i++) {
        try {
            & $agentctl --agentfs $AgentFs init | Out-Null
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $ready) {
        throw "agent-forkd did not become ready"
    }

    & $agentctl --agentfs $AgentFs new -t $EnvId --from $Repo --backend windows-minifilter-overlay -- powershell.exe -NoProfile -Command @'
$ErrorActionPreference = "Stop"
$repo = $env:AGENT_PROJECT_SMOKE_REPO
$readName = $env:AGENT_PROJECT_SMOKE_READ
$writeName = $env:AGENT_PROJECT_SMOKE_WRITE
$deleteName = $env:AGENT_PROJECT_SMOKE_DELETE
$createName = $env:AGENT_PROJECT_SMOKE_CREATE

if ((Get-Location).Path -ne $repo) {
    throw "cwd was not preserved: $((Get-Location).Path)"
}
if ((Get-Content $readName) -ne "host-read-original") {
    throw "managed process could not read lower file from project path"
}
Set-Content $writeName "env-write"
if ((Get-Content $writeName) -ne "env-write") {
    throw "managed process did not read back upper write"
}
Set-Content $createName "env-created"
if ((Get-Content $createName) -ne "env-created") {
    throw "managed process did not read back upper create"
}
Remove-Item $deleteName
if (Test-Path $deleteName) {
    throw "whiteout did not hide deleted lower file"
}
$names = Get-ChildItem -Force -Name
if ($names -notcontains $createName) {
    throw "directory listing did not include upper-created file"
}
if ($names -contains $deleteName) {
    throw "directory listing included whiteouted lower file"
}
if ($names -notcontains $readName) {
    throw "directory listing lost untouched lower file"
}
'@
    if ($LASTEXITCODE -ne 0) {
        throw "agentctl project-path minifilter command failed with exit code $LASTEXITCODE"
    }

    if ((Get-Content $readPath) -ne "host-read-original") {
        throw "host read fixture changed"
    }
    if ((Get-Content $writePath) -ne "host-write-original") {
        throw "host write fixture changed"
    }
    if ((Get-Content $deletePath) -ne "host-delete-original") {
        throw "host delete fixture changed"
    }
    if (Test-Path $createPath) {
        throw "upper-created file leaked into host repo"
    }

    $repoRel = ConvertTo-AgentFsRelativePath $Repo
    $upperRepo = Join-Path (Join-Path $AgentFs "envs\$EnvId\upper") $repoRel
    $whiteoutRepo = Join-Path (Join-Path $AgentFs "envs\$EnvId\whiteouts") $repoRel

    if ((Get-Content (Join-Path $upperRepo $writeName)) -ne "env-write") {
        throw "project write was not redirected to env upper"
    }
    if ((Get-Content (Join-Path $upperRepo $createName)) -ne "env-created") {
        throw "project create was not redirected to env upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutRepo $deleteName))) {
        throw "project delete did not create env whiteout"
    }

    Write-Host "Windows minifilter project-path smoke passed"
} finally {
    if ($daemonProcess -and -not $daemonProcess.HasExited) {
        Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    }
    fltmc unload agentfs 2>$null | Out-Null
    Remove-Item -Force $readPath, $writePath, $deletePath, $createPath -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $AgentFs -ErrorAction SilentlyContinue
    Remove-Item Env:\AGENT_PROJECT_SMOKE_REPO -ErrorAction SilentlyContinue
    Remove-Item Env:\AGENT_PROJECT_SMOKE_READ -ErrorAction SilentlyContinue
    Remove-Item Env:\AGENT_PROJECT_SMOKE_WRITE -ErrorAction SilentlyContinue
    Remove-Item Env:\AGENT_PROJECT_SMOKE_DELETE -ErrorAction SilentlyContinue
    Remove-Item Env:\AGENT_PROJECT_SMOKE_CREATE -ErrorAction SilentlyContinue
}
