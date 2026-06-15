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
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

function Get-WdkBuildVersion {
    $buildRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\build"
    $versions = Get-ChildItem -Directory -Path $buildRoot -ErrorAction Stop |
        Where-Object { Test-Path (Join-Path $_.FullName "WindowsDriver.Common.targets") } |
        Sort-Object Name -Descending
    if (-not $versions) {
        throw "Windows Driver Kit build files were not found under $buildRoot"
    }
    return $versions[0].Name
}

function Install-WdkMsbuildBridge {
    param([string]$WdkVersion)

    $vsRoot = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\2022\BuildTools\MSBuild\Microsoft\VC\v170"
    $toolset = Join-Path $vsRoot "Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0"
    $wdkRoot = (Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\") -replace '\\', '\\'
    New-Item -ItemType Directory -Force -Path $toolset | Out-Null

    @"
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <Import Project="..\v143\Toolset.props" />
  <PropertyGroup>
    <WDKContentRoot>$wdkRoot</WDKContentRoot>
    <WDKBuildFolder Condition="'`$(WDKBuildFolder)' == ''">`$(WindowsTargetPlatformVersion)</WDKBuildFolder>
    <TargetVersion Condition="'`$(TargetVersion)' == ''">Windows10</TargetVersion>
    <TargetPlatformVersion Condition="'`$(TargetPlatformVersion)' == ''">`$(WindowsTargetPlatformVersion)</TargetPlatformVersion>
    <MatchingSdkPresent>true</MatchingSdkPresent>
  </PropertyGroup>
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.Default.props" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.Shared.props" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\x64\WindowsKernelModeDriver\WDK.x64.WindowsKernelModeDriver.props" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.KernelMode.Default.props" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.KernelMode.props" />
</Project>
"@ | Set-Content -Encoding UTF8 -Path (Join-Path $toolset "Toolset.props")

    @"
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <Import Project="..\v143\Toolset.targets" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.Common.targets" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.KernelMode.targets" Condition="Exists('`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\WindowsDriver.KernelMode.targets')" />
  <Import Project="`$(WDKContentRoot)build\`$(WindowsTargetPlatformVersion)\x64\ImportAfter\WDK.x64.WindowsDriverCommonToolset.Platform.Targets" />
</Project>
"@ | Set-Content -Encoding UTF8 -Path (Join-Path $toolset "Toolset.targets")
}

function Get-WdkTool {
    param([string]$WdkVersion, [string]$Name)

    $binRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin\$WdkVersion"
    foreach ($arch in @("x64", "x86", "arm64")) {
        $candidate = Join-Path $binRoot "$arch\$Name"
        if (Test-Path $candidate) {
            return $candidate
        }
    }
    throw "$Name was not found under $binRoot"
}

function Invoke-AgentFsPackageSigning {
    param(
        [string]$DriverDirectory,
        [string]$DriverSys,
        [string]$WdkVersion
    )

    Copy-Item -Force -Path $DriverSys -Destination (Join-Path $DriverDirectory "agentfs.sys")

    $inf2cat = Get-WdkTool -WdkVersion $WdkVersion -Name "inf2cat.exe"
    $signtool = Get-WdkTool -WdkVersion $WdkVersion -Name "signtool.exe"
    Invoke-Logged { & $inf2cat /driver:$DriverDirectory /os:10_X64 }

    $cert = Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq "CN=AgentFs Test Driver" } |
        Sort-Object NotBefore -Descending |
        Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate `
            -Type CodeSigningCert `
            -Subject "CN=AgentFs Test Driver" `
            -CertStoreLocation "Cert:\LocalMachine\My" `
            -HashAlgorithm SHA256
    }

    $cer = Join-Path $DriverDirectory "agentfs-test.cer"
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\Root" | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\TrustedPublisher" | Out-Null

    Invoke-Logged { & $signtool sign /sm /fd SHA256 /sha1 $cert.Thumbprint (Join-Path $DriverDirectory "agentfs.cat") }
    Invoke-Logged { & $signtool sign /sm /fd SHA256 /sha1 $cert.Thumbprint (Join-Path $DriverDirectory "agentfs.sys") }
}

function Install-AgentFsService {
    param([string]$DriverDirectory)

    $driverPath = Join-Path $DriverDirectory "agentfs.sys"
    $systemDriverPath = Join-Path $env:windir "System32\drivers\agentfs.sys"
    Copy-Item -Force -Path $driverPath -Destination $systemDriverPath

    $service = "HKLM:\SYSTEM\CurrentControlSet\Services\agentfs"
    New-Item -Force -Path $service | Out-Null
    New-ItemProperty -Force -Path $service -Name Type -PropertyType DWord -Value 2 | Out-Null
    New-ItemProperty -Force -Path $service -Name Start -PropertyType DWord -Value 3 | Out-Null
    New-ItemProperty -Force -Path $service -Name ErrorControl -PropertyType DWord -Value 1 | Out-Null
    New-ItemProperty -Force -Path $service -Name Group -PropertyType String -Value "FSFilter Activity Monitor" | Out-Null
    New-ItemProperty -Force -Path $service -Name DependOnService -PropertyType MultiString -Value @("FltMgr") | Out-Null
    New-ItemProperty -Force -Path $service -Name ImagePath -PropertyType ExpandString -Value "system32\drivers\agentfs.sys" | Out-Null
    New-ItemProperty -Force -Path $service -Name DisplayName -PropertyType String -Value "IPA-RS AgentFs path-preserving overlay minifilter" | Out-Null

    $instances = Join-Path $service "Instances"
    New-Item -Force -Path $instances | Out-Null
    New-ItemProperty -Force -Path $instances -Name DefaultInstance -PropertyType String -Value "AgentFs Instance" | Out-Null

    $instance = Join-Path $instances "AgentFs Instance"
    New-Item -Force -Path $instance | Out-Null
    New-ItemProperty -Force -Path $instance -Name Altitude -PropertyType String -Value "385240" | Out-Null
    New-ItemProperty -Force -Path $instance -Name Flags -PropertyType DWord -Value 0 | Out-Null
}

function Test-SecureBootEnabled {
    try {
        return [bool](Confirm-SecureBootUEFI)
    } catch {
        return $false
    }
}

function Get-TestSigningState {
    try {
        $bootConfig = bcdedit /enum "{current}" 2>$null
        $line = $bootConfig | Where-Object { $_ -match "testsigning" } | Select-Object -First 1
        if ($line -and $line -match "\s+Yes$") {
            return "on"
        }
        if ($line -and $line -match "\s+No$") {
            return "off"
        }
    } catch {
    }
    return "off"
}

function Invoke-AgentFsLoad {
    Write-Host ">> fltmc load agentfs"
    $output = & fltmc load agentfs 2>&1
    $exitCode = $LASTEXITCODE
    if ($output) {
        $output | ForEach-Object { Write-Host $_ }
    }
    if ($exitCode -eq 0) {
        return
    }

    $secureBoot = Test-SecureBootEnabled
    $testSigning = Get-TestSigningState
    $detail = "fltmc load agentfs failed with exit code $exitCode. SecureBoot=$secureBoot TestSigning=$testSigning."
    if ($secureBoot -and $testSigning -ne "on") {
        $detail += " This smoke script uses a local test certificate; Secure Boot blocks that driver unless test-signing is enabled before boot or the driver is production/attestation signed."
    }
    throw $detail
}

Assert-Admin

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$agentfs = Join-Path $env:TEMP ("agentfs-minifilter-" + [Guid]::NewGuid().ToString("N"))
$source = Join-Path $env:TEMP ("agentfs-source-" + [Guid]::NewGuid().ToString("N"))
$driverProject = Join-Path $repo "drivers\windows-minifilter\agentfs.vcxproj"
$driverSys = Join-Path $repo "drivers\windows-minifilter\$Platform\$Configuration\agentfs.sys"
$driverDir = Split-Path $driverProject
$driverInf = Join-Path $repo "drivers\windows-minifilter\agentfs.inf"
$binDir = Join-Path $repo "target\x86_64-pc-windows-msvc\debug"
$agentctl = Join-Path $binDir "agentctl.exe"
$daemon = Join-Path $binDir "agent-forkd.exe"
$filterctl = Join-Path $binDir "agent-minifilterctl.exe"
$wdkVersion = Get-WdkBuildVersion

try {
    New-Item -ItemType Directory -Force -Path $source | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "nested\lower") | Out-Null
    Set-Content -Path (Join-Path $source "host.txt") -Value "host-original"
    Set-Content -Path (Join-Path $source "delete-me.txt") -Value "delete-original"
    Set-Content -Path (Join-Path $source "nested\lower\deep.txt") -Value "deep-original"

    Push-Location $repo
    Invoke-Logged { cargo build -p agentctl -p agent-forkd -p agent-minifilterctl --target x86_64-pc-windows-msvc }
    Install-WdkMsbuildBridge -WdkVersion $wdkVersion
    Invoke-Logged { msbuild $driverProject /p:Configuration=$Configuration /p:Platform=$Platform /p:WindowsTargetPlatformVersion=$wdkVersion /p:EnableTestSign=false }
    Pop-Location

    if (-not (Test-Path $driverSys)) {
        throw "driver binary was not produced at $driverSys"
    }
    Invoke-AgentFsPackageSigning -DriverDirectory $driverDir -DriverSys $driverSys -WdkVersion $wdkVersion

    Write-Host ">> pnputil /add-driver $driverInf /install"
    & pnputil /add-driver $driverInf /install
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 259) {
        throw "pnputil failed with exit code ${LASTEXITCODE}"
    }
    Install-AgentFsService -DriverDirectory $driverDir
    fltmc unload agentfs 2>$null | Out-Null
    Invoke-AgentFsLoad
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
Set-Content nested\lower\deep.txt 'deep-modified'
New-Item -ItemType Directory -Force -Path nested\created\more | Out-Null
Set-Content nested\created\more\new.txt 'new-deep'
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
    if ((Get-Content (Join-Path $upperSource "nested\lower\deep.txt")) -ne "deep-modified") {
        throw "nested lower file was not copied to upper"
    }
    if ((Get-Content (Join-Path $upperSource "nested\created\more\new.txt")) -ne "new-deep") {
        throw "nested created file was not written to upper"
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
