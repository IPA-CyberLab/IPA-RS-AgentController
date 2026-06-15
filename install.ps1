param(
    [string]$Repo = $(if ($env:AGENT_REPO) { $env:AGENT_REPO } else { "IPA-CyberLab/IPA-RS-IsolatedAgent" }),
    [string]$Version = $(if ($env:AGENT_VERSION) { $env:AGENT_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:AGENT_INSTALL_DIR) { $env:AGENT_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }),
    [string]$Agentfs = $(if ($env:AGENTFS) { $env:AGENTFS } else { Join-Path $HOME ".agentfs" }),
    [switch]$InstallService
)

$ErrorActionPreference = "Stop"

function Get-Target {
    $arch = switch ($env:PROCESSOR_ARCHITECTURE.ToLowerInvariant()) {
        "amd64" { "x86_64" }
        "arm64" { "aarch64" }
        default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
    }
    "$arch-pc-windows-msvc"
}

function Add-UserPath {
    param([string]$PathToAdd)

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @()
    if ($userPath) {
        $entries = $userPath.Split(";") | Where-Object { $_ -ne "" }
    }
    if ($entries -contains $PathToAdd) {
        return
    }
    $newPath = (($entries + $PathToAdd) -join ";")
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    $env:Path = "$env:Path;$PathToAdd"
    Write-Host "Added $PathToAdd to the Windows user PATH"
}

function Install-AgentTask {
    param(
        [string]$InstallDir,
        [string]$Agentfs
    )

    $exe = Join-Path $InstallDir "agent-forkd.exe"
    $taskName = "IPA-RS Isolated Agent"
    $action = New-ScheduledTaskAction -Execute $exe -Argument "--agentfs `"$Agentfs`""
    $trigger = New-ScheduledTaskTrigger -AtLogOn
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Write-Host "Installed and started scheduled task: $taskName"
}

$target = Get-Target
$installServiceRequested = $InstallService.IsPresent -or $env:AGENT_INSTALL_SERVICE -eq "1"
$asset = "ipa-rs-isolated-agent-$target.tar.gz"
if ($Version -eq "latest") {
    $url = "https://github.com/$Repo/releases/latest/download/$asset"
} else {
    $url = "https://github.com/$Repo/releases/download/$Version/$asset"
}

Write-Host "Target: $target"
Write-Host "Release: $Version"
Write-Host "Install dir: $InstallDir"
Write-Host "Install service: $installServiceRequested"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("agent-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    $archive = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $url -OutFile $archive
    tar -xzf $archive -C $tmp

    $payload = Join-Path $tmp "ipa-rs-isolated-agent-$target"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item (Join-Path $payload "bin\agentctl.exe") (Join-Path $InstallDir "agentctl.exe") -Force
    Copy-Item (Join-Path $payload "bin\agentctl.exe") (Join-Path $InstallDir "agctl.exe") -Force
    Copy-Item (Join-Path $payload "bin\agent-forkd.exe") (Join-Path $InstallDir "agent-forkd.exe") -Force
    if (Test-Path (Join-Path $payload "bin\agent-minifilterctl.exe")) {
        Copy-Item (Join-Path $payload "bin\agent-minifilterctl.exe") (Join-Path $InstallDir "agent-minifilterctl.exe") -Force
    }
    if (Test-Path (Join-Path $payload "windows-minifilter")) {
        Copy-Item (Join-Path $payload "windows-minifilter") (Join-Path $InstallDir "windows-minifilter") -Recurse -Force
    }
    Add-UserPath $InstallDir
    if ($installServiceRequested) {
        Install-AgentTask -InstallDir $InstallDir -Agentfs $Agentfs
    }

    Write-Host "Installed agentctl.exe, agctl.exe, agent-forkd.exe, and available Windows overlay helpers to $InstallDir"
    Write-Host "Restart your shell or run: `$env:Path = `"$InstallDir;`$env:Path`""
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
