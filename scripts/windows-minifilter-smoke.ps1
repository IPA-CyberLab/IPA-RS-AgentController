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
    New-Item -ItemType Directory -Force -Path (Join-Path $source "move-lower\inside") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "mixed-lower") | Out-Null
    Set-Content -Path (Join-Path $source "host.txt") -Value "host-original"
    Set-Content -Path (Join-Path $source "delete-me.txt") -Value "delete-original"
    Set-Content -Path (Join-Path $source "recreate-me.txt") -Value "recreate-original"
    Set-Content -Path (Join-Path $source "rename-target.txt") -Value "rename-target-original"
    Set-Content -Path (Join-Path $source "metadata.txt") -Value "metadata-original"
    (Get-Item (Join-Path $source "metadata.txt")).LastWriteTimeUtc = [DateTimeOffset]::Parse("2019-01-02T03:04:05Z").UtcDateTime
    Set-Content -Path (Join-Path $source "collision-source.txt") -Value "collision-source-original"
    Set-Content -Path (Join-Path $source "collision-target.txt") -Value "collision-target-original"
    Set-Content -Path (Join-Path $source "nested\lower\deep.txt") -Value "deep-original"
    Set-Content -Path (Join-Path $source "move-lower\inside\lower-file.txt") -Value "lower-tree-original"
    Set-Content -Path (Join-Path $source "mixed-lower\upper-changed.txt") -Value "mixed-lower-original"
    Set-Content -Path (Join-Path $source "mixed-lower\lower-only.txt") -Value "mixed-lower-only"

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
(Get-Item metadata.txt).LastWriteTimeUtc = [DateTimeOffset]::Parse('2020-02-03T04:05:06Z').UtcDateTime
Set-Content host.txt 'env-modified'
Set-Content created.txt 'env-created'
New-Item -ItemType Directory -Force -Path upper-only-dir | Out-Null
Set-Content upper-only-dir\child.txt 'upper-only-child'
if ((Get-Content upper-only-dir\child.txt) -ne 'upper-only-child') { throw 'upper-only directory child read failed' }
if ((Get-ChildItem upper-only-dir -Name) -notcontains 'child.txt') { throw 'upper-only directory listing lost child' }
New-Item -ItemType Directory -Force -Path stale-dir | Out-Null
Set-Content stale-dir\old.txt 'stale-upper-child'
Remove-Item -Recurse stale-dir
New-Item -ItemType Directory -Force -Path stale-dir | Out-Null
if ((Get-ChildItem stale-dir -Force | Measure-Object).Count -ne 0) { throw 'recreated upper directory exposed stale deleted child' }
`$renameCollisionFailed = `$false
try {
    Rename-Item collision-source.txt collision-target.txt
} catch {
    `$renameCollisionFailed = `$true
}
if (-not `$renameCollisionFailed) { throw 'rename over existing lower target unexpectedly succeeded' }
if ((Get-Content collision-source.txt) -ne 'collision-source-original') { throw 'rename collision hid source file' }
if ((Get-Content collision-target.txt) -ne 'collision-target-original') { throw 'rename collision modified target file' }
New-Item -ItemType Directory -Force -Path nested\lower | Out-Null
Set-Content nested\lower\deep.txt 'deep-modified'
New-Item -ItemType Directory -Force -Path nested\created\more | Out-Null
Set-Content nested\created\more\new.txt 'new-deep'
Rename-Item nested\created nested\renamed
Set-Content mixed-lower\upper-changed.txt 'mixed-upper-modified'
Rename-Item mixed-lower mixed-renamed
Rename-Item move-lower moved-lower
Remove-Item delete-me.txt
Remove-Item recreate-me.txt
Set-Content recreate-me.txt 'recreated-in-env'
Remove-Item rename-target.txt
Rename-Item created.txt rename-target.txt
`$names = Get-ChildItem -Name | Sort-Object
if (`$names -notcontains 'host.txt') { throw 'directory listing lost lower file' }
if (`$names -notcontains 'mixed-renamed') { throw 'directory listing lost renamed mixed directory' }
if (`$names -notcontains 'rename-target.txt') { throw 'directory listing lost upper file renamed onto deleted target' }
if (`$names -notcontains 'moved-lower') { throw 'directory listing lost renamed lower directory' }
if (`$names -notcontains 'recreate-me.txt') { throw 'directory listing lost recreated file' }
if (`$names -notcontains 'stale-dir') { throw 'directory listing lost recreated upper directory' }
if (`$names -notcontains 'upper-only-dir') { throw 'directory listing lost upper-only directory' }
if (`$names -contains 'delete-me.txt') { throw 'directory listing showed whiteout file' }
if (`$names -contains 'mixed-lower') { throw 'directory listing showed renamed mixed source' }
if (`$names -contains 'move-lower') { throw 'directory listing showed renamed lower source' }
"@

    $hostContent = Get-Content (Join-Path $source "host.txt")
    if ($hostContent -ne "host-original") {
        throw "host file was modified: $hostContent"
    }
    if (-not (Test-Path (Join-Path $source "delete-me.txt"))) {
        throw "host delete-me.txt was removed"
    }
    if ((Get-Content (Join-Path $source "recreate-me.txt")) -ne "recreate-original") {
        throw "host recreate-me.txt was modified"
    }
    if ((Get-Content (Join-Path $source "rename-target.txt")) -ne "rename-target-original") {
        throw "host rename-target.txt was modified"
    }
    if ((Get-Item (Join-Path $source "metadata.txt")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2019-01-02T03:04:05Z").UtcDateTime) {
        throw "host metadata.txt timestamp was modified"
    }
    if ((Get-Content (Join-Path $source "collision-source.txt")) -ne "collision-source-original") {
        throw "host collision-source.txt was modified"
    }
    if ((Get-Content (Join-Path $source "collision-target.txt")) -ne "collision-target-original") {
        throw "host collision-target.txt was modified"
    }
    if ((Get-Content (Join-Path $source "move-lower\inside\lower-file.txt")) -ne "lower-tree-original") {
        throw "host move-lower tree was modified"
    }
    if ((Get-Content (Join-Path $source "mixed-lower\upper-changed.txt")) -ne "mixed-lower-original") {
        throw "host mixed-lower upper-changed.txt was modified"
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
    if ((Get-Content (Join-Path $upperSource "nested\renamed\more\new.txt")) -ne "new-deep") {
        throw "nested renamed directory contents were not written to upper"
    }
    if (Test-Path (Join-Path $upperSource "nested\created")) {
        throw "renamed upper directory source still exists"
    }
    if ((Get-Content (Join-Path $upperSource "moved-lower\inside\lower-file.txt")) -ne "lower-tree-original") {
        throw "renamed lower directory tree was not copied to upper"
    }
    if ((Get-Content (Join-Path $upperSource "mixed-renamed\upper-changed.txt")) -ne "mixed-upper-modified") {
        throw "renamed mixed directory lost upper-modified file"
    }
    if ((Get-Content (Join-Path $upperSource "mixed-renamed\lower-only.txt")) -ne "mixed-lower-only") {
        throw "renamed mixed directory lost lower-only file"
    }
    if ((Get-Content (Join-Path $upperSource "rename-target.txt")) -ne "env-created") {
        throw "file renamed onto deleted target was not written to upper"
    }
    if ((Get-Content (Join-Path $upperSource "recreate-me.txt")) -ne "recreated-in-env") {
        throw "recreated file was not written to upper"
    }
    if (-not (Test-Path (Join-Path $upperSource "stale-dir"))) {
        throw "recreated upper directory was not present in upper"
    }
    if (Test-Path (Join-Path $upperSource "stale-dir\old.txt")) {
        throw "deleted upper directory child was left behind"
    }
    if ((Get-Content (Join-Path $upperSource "upper-only-dir\child.txt")) -ne "upper-only-child") {
        throw "upper-only directory child was not written to upper"
    }
    if ((Get-Item (Join-Path $upperSource "metadata.txt")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2020-02-03T04:05:06Z").UtcDateTime) {
        throw "metadata write was not redirected to upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "delete-me.txt"))) {
        throw "delete whiteout was not created"
    }
    if (Test-Path (Join-Path $whiteoutSource "collision-source.txt")) {
        throw "failed rename collision created a source whiteout"
    }
    if (Test-Path (Join-Path $upperSource "collision-target.txt")) {
        throw "failed rename collision wrote the target to upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "move-lower"))) {
        throw "renamed lower directory source whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "mixed-lower"))) {
        throw "renamed mixed directory source whiteout was not created"
    }
    if (Test-Path (Join-Path $whiteoutSource "rename-target.txt")) {
        throw "rename target still has a whiteout"
    }
    if (Test-Path (Join-Path $whiteoutSource "recreate-me.txt")) {
        throw "recreated file still has a whiteout"
    }

    Write-Host "Windows minifilter smoke passed"
} finally {
    if ($daemonProcess -and -not $daemonProcess.HasExited) {
        Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    }
    fltmc unload agentfs 2>$null | Out-Null
    Remove-Item -Recurse -Force $agentfs, $source -ErrorAction SilentlyContinue
}
