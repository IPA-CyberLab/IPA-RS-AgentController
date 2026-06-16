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

function Add-AgentFsEaType {
    if ("AgentFsEa" -as [type]) {
        return
    }

    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class AgentFsEa {
    private const uint GENERIC_READ = 0x80000000;
    private const uint GENERIC_WRITE = 0x40000000;
    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_ATTRIBUTE_NORMAL = 0x00000080;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_STATUS_BLOCK {
        public IntPtr Status;
        public UIntPtr Information;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("ntdll.dll")]
    private static extern int NtSetEaFile(SafeFileHandle fileHandle, out IO_STATUS_BLOCK ioStatusBlock, byte[] buffer, uint length);

    [DllImport("ntdll.dll")]
    private static extern int NtQueryEaFile(
        SafeFileHandle fileHandle,
        out IO_STATUS_BLOCK ioStatusBlock,
        byte[] buffer,
        uint length,
        bool returnSingleEntry,
        IntPtr eaList,
        uint eaListLength,
        IntPtr eaIndex,
        bool restartScan);

    private static SafeFileHandle OpenEaHandle(string path, bool writable, bool directory) {
        uint access = writable ? (GENERIC_READ | GENERIC_WRITE) : GENERIC_READ;
        uint flags = FILE_ATTRIBUTE_NORMAL;
        if (directory) {
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }

        SafeFileHandle handle = CreateFile(
            path,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            flags,
            IntPtr.Zero);
        if (handle.IsInvalid) {
            throw new IOException("CreateFile failed: " + Marshal.GetLastWin32Error());
        }
        return handle;
    }

    private static void SetEaInternal(string path, string name, string value, bool directory) {
        byte[] nameBytes = Encoding.ASCII.GetBytes(name);
        byte[] valueBytes = Encoding.UTF8.GetBytes(value);
        byte[] buffer = new byte[8 + nameBytes.Length + 1 + valueBytes.Length];
        buffer[5] = checked((byte)nameBytes.Length);
        BitConverter.GetBytes((ushort)valueBytes.Length).CopyTo(buffer, 6);
        Buffer.BlockCopy(nameBytes, 0, buffer, 8, nameBytes.Length);
        Buffer.BlockCopy(valueBytes, 0, buffer, 8 + nameBytes.Length + 1, valueBytes.Length);

        using (SafeFileHandle handle = OpenEaHandle(path, true, directory)) {
            IO_STATUS_BLOCK iosb;
            int status = NtSetEaFile(handle, out iosb, buffer, (uint)buffer.Length);
            if (status < 0) {
                throw new InvalidOperationException("NtSetEaFile failed: 0x" + status.ToString("X8"));
            }
        }
    }

    private static string GetEaInternal(string path, string name, bool directory) {
        byte[] buffer = new byte[65536];
        using (SafeFileHandle handle = OpenEaHandle(path, false, directory)) {
            IO_STATUS_BLOCK iosb;
            int status = NtQueryEaFile(handle, out iosb, buffer, (uint)buffer.Length, false, IntPtr.Zero, 0, IntPtr.Zero, true);
            if (status < 0) {
                return null;
            }
        }

        int offset = 0;
        while (offset + 8 <= buffer.Length) {
            int next = BitConverter.ToInt32(buffer, offset);
            int nameLength = buffer[offset + 5];
            int valueLength = BitConverter.ToUInt16(buffer, offset + 6);
            if (nameLength == 0 || offset + 8 + nameLength + 1 + valueLength > buffer.Length) {
                return null;
            }
            string entryName = Encoding.ASCII.GetString(buffer, offset + 8, nameLength);
            if (String.Equals(entryName, name, StringComparison.OrdinalIgnoreCase)) {
                return Encoding.UTF8.GetString(buffer, offset + 8 + nameLength + 1, valueLength);
            }
            if (next == 0) {
                break;
            }
            offset += next;
        }
        return null;
    }

    public static void SetEa(string path, string name, string value) {
        SetEaInternal(path, name, value, false);
    }

    public static string GetEa(string path, string name) {
        return GetEaInternal(path, name, false);
    }

    public static void SetDirectoryEa(string path, string name, string value) {
        SetEaInternal(path, name, value, true);
    }

    public static string GetDirectoryEa(string path, string name) {
        return GetEaInternal(path, name, true);
    }
}
'@
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
Add-AgentFsEaType

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$agentfs = Join-Path $env:TEMP ("agentfs-minifilter-" + [Guid]::NewGuid().ToString("N"))
$source = Join-Path $env:TEMP ("agentfs-source-" + [Guid]::NewGuid().ToString("N"))
$outsideMoveSource = Join-Path $env:TEMP ("agentfs-outside-move-" + [Guid]::NewGuid().ToString("N") + ".txt")
$outsideLinkSource = Join-Path $env:TEMP ("agentfs-outside-link-" + [Guid]::NewGuid().ToString("N") + ".txt")
$driverProject = Join-Path $repo "drivers\windows-minifilter\agentfs.vcxproj"
$driverSys = Join-Path $repo "drivers\windows-minifilter\$Platform\$Configuration\agentfs.sys"
$driverDir = Split-Path $driverProject
$driverInf = Join-Path $repo "drivers\windows-minifilter\agentfs.inf"
$binDir = Join-Path $repo "target\x86_64-pc-windows-msvc\debug"
$agentctl = Join-Path $binDir "agentctl.exe"
$daemon = Join-Path $binDir "agent-forkd.exe"
$filterctl = Join-Path $binDir "agent-minifilterctl.exe"
$wdkVersion = Get-WdkBuildVersion
$lockedStream = $null

try {
    New-Item -ItemType Directory -Force -Path $source | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "nested\lower") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "delete-lower-dir\child") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "move-lower\inside") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "metadata-dir") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "mixed-lower") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "CaseDirDelete\child") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $source "CaseDirRename\child") | Out-Null
    Set-Content -Path (Join-Path $source "host.txt") -Value "host-original"
    Set-Content -Path (Join-Path $source "CaseSource.TXT") -Value "case-original"
    Set-Content -Path (Join-Path $source "CaseDelete.TXT") -Value "case-delete-original"
    Set-Content -Path (Join-Path $source "CaseRename.TXT") -Value "case-rename-original"
    Set-Content -Path (Join-Path $source "CaseDirDelete\child\lower-file.txt") -Value "case-dir-delete-original"
    Set-Content -Path (Join-Path $source "CaseDirRename\child\lower-file.txt") -Value "case-dir-rename-original"
    Set-Content -Path (Join-Path $source "delete-me.txt") -Value "delete-original"
    $readonlyDelete = Join-Path $source "readonly-delete.txt"
    Set-Content -Path $readonlyDelete -Value "readonly-delete-original"
    $readonlyDeleteItem = Get-Item $readonlyDelete
    $readonlyDeleteItem.Attributes = $readonlyDeleteItem.Attributes -bor [IO.FileAttributes]::ReadOnly
    $readonlyAttributes = Join-Path $source "readonly-attributes.txt"
    Set-Content -Path $readonlyAttributes -Value "readonly-attributes-original"
    $readonlyAttributesItem = Get-Item $readonlyAttributes
    $readonlyAttributesItem.Attributes = $readonlyAttributesItem.Attributes -bor [IO.FileAttributes]::ReadOnly
    Set-Content -Path (Join-Path $source "recreate-me.txt") -Value "recreate-original"
    Set-Content -Path (Join-Path $source "rename-target.txt") -Value "rename-target-original"
    Set-Content -Path (Join-Path $source "metadata.txt") -Value "metadata-original"
    (Get-Item (Join-Path $source "metadata.txt")).LastWriteTimeUtc = [DateTimeOffset]::Parse("2019-01-02T03:04:05Z").UtcDateTime
    Set-Content -Path (Join-Path $source "metadata-dir\child.txt") -Value "metadata-dir-child-original"
    [AgentFsEa]::SetDirectoryEa((Join-Path $source "metadata-dir"), "agentfs.metadata.dir.ea", "lower-metadata-dir-ea")
    (Get-Item (Join-Path $source "metadata-dir")).LastWriteTimeUtc = [DateTimeOffset]::Parse("2016-01-02T03:04:05Z").UtcDateTime
    Set-Content -Path (Join-Path $source "truncate.txt") -Value "truncate-original"
    Set-Content -Path (Join-Path $source "append.txt") -Value "append-original"
    Set-Content -Path (Join-Path $source "overwrite.txt") -Value "overwrite-original"
    [IO.File]::WriteAllText((Join-Path $source "open-or-create-existing.txt"), "open-or-create-existing-original")
    [IO.File]::WriteAllText((Join-Path $source "create-new-existing.txt"), "create-new-existing-original")
    [IO.File]::WriteAllText((Join-Path $source "mapped.txt"), "0000000000")
    Set-Content -Path (Join-Path $source "locked.txt") -Value "locked-original"
    Set-Content -Path (Join-Path $source "ea-source.txt") -Value "ea-main-original"
    [AgentFsEa]::SetEa((Join-Path $source "ea-source.txt"), "agentfs.ea", "lower-ea-original")
    Set-Content -Path (Join-Path $source "stream-source.txt") -Value "stream-main-original"
    Set-Content -Path (Join-Path $source "stream-source.txt") -Stream lower -Value "lower-stream-original"
    $aclSource = Join-Path $source "acl-source.txt"
    Set-Content -Path $aclSource -Value "acl-original"
    $acl = Get-Acl $aclSource
    $acl.SetAccessRuleProtection($true, $false)
    $currentUser = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $administrators = [Security.Principal.SecurityIdentifier]::new("S-1-5-32-544")
    $acl.SetOwner($currentUser)
    $acl.SetGroup($currentUser)
    $acl.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new($currentUser, "FullControl", "Allow"))
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($administrators, "FullControl", "Allow")) | Out-Null
    Set-Acl -Path $aclSource -AclObject $acl
    $aclSourceSddl = (Get-Acl $aclSource).Sddl
    $aclChangeSource = Join-Path $source "acl-change-source.txt"
    Set-Content -Path $aclChangeSource -Value "acl-change-original"
    $aclChange = Get-Acl $aclChangeSource
    $aclChange.SetAccessRuleProtection($true, $false)
    $aclChange.SetOwner($currentUser)
    $aclChange.SetGroup($currentUser)
    $aclChange.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new($currentUser, "FullControl", "Allow"))
    $aclChange.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($administrators, "FullControl", "Allow")) | Out-Null
    Set-Acl -Path $aclChangeSource -AclObject $aclChange
    $aclChangeSourceSddl = (Get-Acl $aclChangeSource).Sddl
    $aclChangeExpected = Get-Acl $aclChangeSource
    $aclChangeExpected.PurgeAccessRules($administrators)
    $aclChangeExpectedSddl = $aclChangeExpected.Sddl
    $ownerChangeSource = Join-Path $source "owner-change-source.txt"
    Set-Content -Path $ownerChangeSource -Value "owner-change-original"
    $ownerChange = Get-Acl $ownerChangeSource
    $ownerChange.SetAccessRuleProtection($true, $false)
    $ownerChange.SetOwner($currentUser)
    $ownerChange.SetGroup($currentUser)
    $ownerChange.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new($currentUser, "FullControl", "Allow"))
    $ownerChange.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($administrators, "FullControl", "Allow")) | Out-Null
    Set-Acl -Path $ownerChangeSource -AclObject $ownerChange
    $ownerChangeSourceSddl = (Get-Acl $ownerChangeSource).Sddl
    $ownerChangeExpected = Get-Acl $ownerChangeSource
    $ownerChangeExpected.SetOwner($administrators)
    $ownerChangeExpectedSddl = $ownerChangeExpected.Sddl
    Set-Content -Path (Join-Path $source "collision-source.txt") -Value "collision-source-original"
    Set-Content -Path (Join-Path $source "collision-target.txt") -Value "collision-target-original"
    Set-Content -Path (Join-Path $source "replace-file-source.txt") -Value "replace-file-source-original"
    Set-Content -Path (Join-Path $source "replace-file-target.txt") -Value "replace-file-target-original"
    Set-Content -Path (Join-Path $source "replace-dir-source.txt") -Value "replace-dir-source-original"
    New-Item -ItemType Directory -Force -Path (Join-Path $source "replace-dir-target") | Out-Null
    Set-Content -Path (Join-Path $source "replace-dir-target\child.txt") -Value "replace-dir-target-original"
    New-Item -ItemType SymbolicLink -Path (Join-Path $source "lower-symlink.txt") -Target (Join-Path $source "host.txt") | Out-Null
    Set-Content -Path (Join-Path $source "nested\lower\deep.txt") -Value "deep-original"
    Set-Content -Path (Join-Path $source "delete-lower-dir\child\lower-file.txt") -Value "delete-lower-dir-original"
    Set-Content -Path (Join-Path $source "move-lower\inside\lower-file.txt") -Value "lower-tree-original"
    (Get-Item (Join-Path $source "move-lower\inside\lower-file.txt")).LastWriteTimeUtc = [DateTimeOffset]::Parse("2018-07-08T09:10:11Z").UtcDateTime
    $moveLower = Join-Path $source "move-lower"
    [AgentFsEa]::SetDirectoryEa($moveLower, "agentfs.dir.ea", "lower-dir-ea-original")
    $moveLowerAcl = Get-Acl $moveLower
    $moveLowerAcl.SetAccessRuleProtection($true, $false)
    $moveLowerAcl.SetOwner($currentUser)
    $moveLowerAcl.SetGroup($currentUser)
    $moveLowerAcl.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new($currentUser, "FullControl", "ContainerInherit,ObjectInherit", "None", "Allow"))
    $moveLowerAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($administrators, "FullControl", "ContainerInherit,ObjectInherit", "None", "Allow")) | Out-Null
    Set-Acl -Path $moveLower -AclObject $moveLowerAcl
    (Get-Item $moveLower).LastWriteTimeUtc = [DateTimeOffset]::Parse("2017-06-05T04:03:02Z").UtcDateTime
    $moveLowerSddl = (Get-Acl $moveLower).Sddl
    Set-Content -Path (Join-Path $source "mixed-lower\upper-changed.txt") -Value "mixed-lower-original"
    Set-Content -Path (Join-Path $source "mixed-lower\lower-only.txt") -Value "mixed-lower-only"
    Set-Content -Path $outsideMoveSource -Value "outside-move-original"
    Set-Content -Path $outsideLinkSource -Value "outside-link-original"

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

    $lockedPath = Join-Path $source "locked.txt"
    $lockedStream = [IO.File]::Open($lockedPath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)

    & $agentctl --agentfs $agentfs new -t $EnvId --from $source -- powershell.exe -NoProfile -Command @"
`$ErrorActionPreference = 'Stop'
if ((Get-Location).Path -ne '$source') { throw "cwd was not preserved: `$((Get-Location).Path)" }
if ((Get-Content host.txt) -ne 'host-original') { throw 'lower read failed' }
`$lockedWriteFailed = `$false
try {
    Set-Content locked.txt 'locked-env'
} catch {
    `$lockedWriteFailed = `$true
}
if (-not `$lockedWriteFailed) { throw 'write to exclusively locked lower file unexpectedly succeeded' }
`$lockedDeleteFailed = `$false
try {
    Remove-Item locked.txt
} catch {
    `$lockedDeleteFailed = `$true
}
if (-not `$lockedDeleteFailed) { throw 'delete of exclusively locked lower file unexpectedly succeeded' }
if (-not (Test-Path locked.txt)) { throw 'failed locked lower delete hid the file' }
(Get-Item metadata.txt).LastWriteTimeUtc = [DateTimeOffset]::Parse('2020-02-03T04:05:06Z').UtcDateTime
(Get-Item metadata-dir).LastWriteTimeUtc = [DateTimeOffset]::Parse('2021-03-04T05:06:07Z').UtcDateTime
[AgentFsEa]::SetDirectoryEa((Join-Path (Get-Location) 'metadata-dir'), 'agentfs.metadata.dir.ea', 'env-dir-ea')
if ([AgentFsEa]::GetDirectoryEa((Join-Path (Get-Location) 'metadata-dir'), 'agentfs.metadata.dir.ea') -ne 'env-dir-ea') { throw 'directory EA write readback failed' }
if ((Get-ChildItem metadata-dir -Name) -notcontains 'child.txt') { throw 'directory metadata copy-up hid lower child' }
`$truncatePath = Join-Path (Get-Location) 'truncate.txt'
`$truncateFile = [IO.File]::Open(`$truncatePath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
try {
    `$truncateFile.SetLength(8)
} finally {
    `$truncateFile.Dispose()
}
if ([IO.File]::ReadAllText(`$truncatePath) -ne 'truncate') { throw 'truncated lower file did not show env length' }
Add-Content append.txt 'append-env'
if (((Get-Content append.txt) -join "`n") -ne "append-original`nappend-env") { throw 'append write was not visible in env' }
`$overwritePath = Join-Path (Get-Location) 'overwrite.txt'
`$overwriteBytes = [Text.Encoding]::UTF8.GetBytes('overwrite-env')
`$overwriteFile = [IO.File]::Open(`$overwritePath, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
try {
    foreach (`$byte in `$overwriteBytes) {
        `$overwriteFile.WriteByte(`$byte)
    }
} finally {
    `$overwriteFile.Dispose()
}
if ([IO.File]::ReadAllText(`$overwritePath) -ne 'overwrite-env') { throw 'overwrite create disposition was not visible in env' }
`$openIfExistingPath = Join-Path (Get-Location) 'open-or-create-existing.txt'
`$openIfExistingBytes = [Text.Encoding]::UTF8.GetBytes('open-or-create-existing-env')
`$openIfExistingFile = [IO.File]::Open(`$openIfExistingPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
try {
    `$openIfExistingFile.SetLength(0)
    `$openIfExistingFile.Write(`$openIfExistingBytes, 0, `$openIfExistingBytes.Length)
} finally {
    `$openIfExistingFile.Dispose()
}
if ([IO.File]::ReadAllText(`$openIfExistingPath) -ne 'open-or-create-existing-env') { throw 'open-or-create existing lower file was not visible in env' }
`$openIfNewPath = Join-Path (Get-Location) 'open-or-create-new.txt'
`$openIfNewBytes = [Text.Encoding]::UTF8.GetBytes('open-or-create-new-env')
`$openIfNewFile = [IO.File]::Open(`$openIfNewPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
try {
    `$openIfNewFile.Write(`$openIfNewBytes, 0, `$openIfNewBytes.Length)
} finally {
    `$openIfNewFile.Dispose()
}
if ([IO.File]::ReadAllText(`$openIfNewPath) -ne 'open-or-create-new-env') { throw 'open-or-create new file was not visible in env' }
`$createNewExistingFailed = `$false
try {
    `$createNewExisting = [IO.File]::Open((Join-Path (Get-Location) 'create-new-existing.txt'), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
    `$createNewExisting.Dispose()
} catch {
    `$createNewExistingFailed = `$true
}
if (-not `$createNewExistingFailed) { throw 'create-new existing lower file unexpectedly succeeded' }
`$createNewPath = Join-Path (Get-Location) 'create-new.txt'
`$createNewBytes = [Text.Encoding]::UTF8.GetBytes('create-new-env')
`$createNewFile = [IO.File]::Open(`$createNewPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
try {
    `$createNewFile.Write(`$createNewBytes, 0, `$createNewBytes.Length)
} finally {
    `$createNewFile.Dispose()
}
if ([IO.File]::ReadAllText(`$createNewPath) -ne 'create-new-env') { throw 'create-new file was not visible in env' }
`$readonlyAttributesItem = Get-Item readonly-attributes.txt
`$readonlyAttributesItem.Attributes = `$readonlyAttributesItem.Attributes -band (-bnot [IO.FileAttributes]::ReadOnly)
Set-Content readonly-attributes.txt 'readonly-attributes-env'
if ((Get-Content readonly-attributes.txt) -ne 'readonly-attributes-env') { throw 'readonly attribute copy-up write readback failed' }
if (((Get-Item readonly-attributes.txt).Attributes -band [IO.FileAttributes]::ReadOnly) -ne 0) { throw 'readonly attribute clear was not visible in env' }
Set-Content host.txt 'env-modified'
Set-Content casesource.txt 'case-env'
if ((Get-Content casesource.txt) -ne 'case-env') { throw 'case-insensitive lower write readback failed' }
Set-Content created.txt 'env-created'
& powershell.exe -NoProfile -Command "Set-Content child-process.txt 'child-env'; & powershell.exe -NoProfile -EncodedCommand UwBlAHQALQBDAG8AbgB0AGUAbgB0ACAAZwByAGEAbgBkAGMAaABpAGwAZAAtAHAAcgBvAGMAZQBzAHMALgB0AHgAdAAgACcAZwByAGEAbgBkAGMAaABpAGwAZAAtAGUAbgB2ACcAOwAgAGkAZgAgACgAKABHAGUAdAAtAEMAbwBuAHQAZQBuAHQAIABnAHIAYQBuAGQAYwBoAGkAbABkAC0AcAByAG8AYwBlAHMAcwAuAHQAeAB0ACkAIAAtAG4AZQAgACcAZwByAGEAbgBkAGMAaABpAGwAZAAtAGUAbgB2ACcAKQAgAHsAIAB0AGgAcgBvAHcAIAAnAGcAcgBhAG4AZABjAGgAaQBsAGQAIABwAHIAbwBjAGUAcwBzACAAdwByAGkAdABlACAAcgBlAGEAZABiAGEAYwBrACAAZgBhAGkAbABlAGQAJwAgAH0A; if (`$LASTEXITCODE -ne 0) { throw 'grandchild process overlay command failed' }; if ((Get-Content child-process.txt) -ne 'child-env') { throw 'child process write readback failed' }"
if (`$LASTEXITCODE -ne 0) { throw 'child process overlay command failed' }
Set-Content acl-source.txt 'acl-env'
`$aclChange = Get-Acl acl-change-source.txt
`$aclChange.PurgeAccessRules([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
Set-Acl -Path acl-change-source.txt -AclObject `$aclChange
`$ownerChange = Get-Acl owner-change-source.txt
`$ownerChange.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
Set-Acl -Path owner-change-source.txt -AclObject `$ownerChange
`$mappedPath = Join-Path (Get-Location) 'mapped.txt'
`$mappedBytes = [Text.Encoding]::UTF8.GetBytes('mapped-env')
`$mappedFile = [IO.File]::Open(`$mappedPath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
try {
    `$mappedName = 'agentfs-smoke-' + [Guid]::NewGuid().ToString('N')
    `$mmf = [IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile(`$mappedFile, `$mappedName, 0, [IO.MemoryMappedFiles.MemoryMappedFileAccess]::ReadWrite, `$null, [IO.HandleInheritability]::None, `$false)
    try {
        `$view = `$mmf.CreateViewAccessor(0, `$mappedBytes.Length, [IO.MemoryMappedFiles.MemoryMappedFileAccess]::Write)
        try {
            `$view.WriteArray(0, `$mappedBytes, 0, `$mappedBytes.Length)
            `$view.Flush()
        } finally {
            `$view.Dispose()
        }
    } finally {
        `$mmf.Dispose()
    }
} finally {
    `$mappedFile.Dispose()
}
Set-Content ea-source.txt 'ea-main-env'
[AgentFsEa]::SetEa((Join-Path (Get-Location) 'ea-source.txt'), 'agentfs.ea', 'env-ea')
if ([AgentFsEa]::GetEa((Join-Path (Get-Location) 'ea-source.txt'), 'agentfs.ea') -ne 'env-ea') { throw 'EA write readback failed' }
if ((Get-Content stream-source.txt -Stream lower) -ne 'lower-stream-original') { throw 'lower ADS read failed' }
Set-Content stream-source.txt 'stream-main-env'
Set-Content stream-source.txt -Stream lower 'lower-stream-env'
if ((Get-Content stream-source.txt -Stream lower) -ne 'lower-stream-env') { throw 'lower ADS write readback failed' }
Set-Content stream-source.txt -Stream env 'env-stream'
`$hardlinkFailed = `$false
try {
    New-Item -ItemType HardLink -Path hardlink-host.txt -Target host.txt | Out-Null
} catch {
    `$hardlinkFailed = `$true
}
if (-not `$hardlinkFailed) { throw 'hardlink creation unexpectedly succeeded inside overlay' }
`$symlinkFailed = `$false
try {
    New-Item -ItemType SymbolicLink -Path symlink-host.txt -Target host.txt | Out-Null
} catch {
    `$symlinkFailed = `$true
}
if (-not `$symlinkFailed) { throw 'symlink creation unexpectedly succeeded inside overlay' }
`$lowerSymlinkWriteFailed = `$false
try {
    Set-Content lower-symlink.txt 'lower-symlink-env'
} catch {
    `$lowerSymlinkWriteFailed = `$true
}
if (-not `$lowerSymlinkWriteFailed) { throw 'lower symlink write unexpectedly succeeded inside overlay' }
`$lowerSymlinkRenameFailed = `$false
try {
    Rename-Item lower-symlink.txt moved-lower-symlink.txt
} catch {
    `$lowerSymlinkRenameFailed = `$true
}
if (-not `$lowerSymlinkRenameFailed) { throw 'lower symlink rename unexpectedly succeeded inside overlay' }
Remove-Item lower-symlink.txt
if (Test-Path lower-symlink.txt) { throw 'lower symlink delete left source visible' }
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
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Collections.Generic;
using Microsoft.Win32.SafeHandles;

public static class AgentFsNativeMove {
    private const uint FILE_LIST_DIRECTORY = 0x00000001;
    private const uint DELETE_ACCESS = 0x00010000;
    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_ATTRIBUTE_NORMAL = 0x00000080;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    private const uint FILE_DISPOSITION_DELETE = 0x00000001;
    private const uint FILE_DISPOSITION_ON_CLOSE = 0x00000008;
    private const uint FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE = 0x00000010;
    private const uint FILE_RENAME_REPLACE_IF_EXISTS = 0x00000001;
    private const uint FILE_RENAME_IGNORE_READONLY_ATTRIBUTE = 0x00000040;
    private const int FileDispositionInfoEx = 21;
    private const int FileRenameInfoEx = 22;
    private const int STATUS_NO_MORE_FILES = unchecked((int)0x80000006);
    private const int STATUS_NO_SUCH_FILE = unchecked((int)0xC000000F);

    [StructLayout(LayoutKind.Sequential)]
    private struct FILE_DISPOSITION_INFO_EX {
        public uint Flags;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_STATUS_BLOCK {
        public IntPtr Status;
        public UIntPtr Information;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct UNICODE_STRING {
        public ushort Length;
        public ushort MaximumLength;
        public IntPtr Buffer;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool MoveFileEx(string existingFileName, string newFileName, int flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool CreateHardLink(string fileName, string existingFileName, IntPtr securityAttributes);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetFileInformationByHandle(
        SafeFileHandle fileHandle,
        int fileInformationClass,
        ref FILE_DISPOSITION_INFO_EX fileInformation,
        int bufferSize);

    [DllImport("kernel32.dll", EntryPoint = "SetFileInformationByHandle", SetLastError = true)]
    private static extern bool SetFileInformationByHandleBuffer(
        SafeFileHandle fileHandle,
        int fileInformationClass,
        byte[] fileInformation,
        int bufferSize);

    [DllImport("ntdll.dll")]
    private static extern int NtQueryDirectoryFile(
        SafeFileHandle fileHandle,
        IntPtr eventHandle,
        IntPtr apcRoutine,
        IntPtr apcContext,
        out IO_STATUS_BLOCK ioStatusBlock,
        byte[] fileInformation,
        uint length,
        int fileInformationClass,
        bool returnSingleEntry,
        IntPtr fileName,
        bool restartScan);

    public static string[] QueryDirectoryNames(string path, int fileInformationClass, int fileNameLengthOffset, int fileNameOffset) {
        return QueryDirectoryNames(path, fileInformationClass, fileNameLengthOffset, fileNameOffset, false);
    }

    public static string[] QueryDirectoryNamesSingleEntry(string path, int fileInformationClass, int fileNameLengthOffset, int fileNameOffset) {
        return QueryDirectoryNames(path, fileInformationClass, fileNameLengthOffset, fileNameOffset, true);
    }

    public static string[] QueryDirectoryNamesPattern(string path, string pattern, int fileInformationClass, int fileNameLengthOffset, int fileNameOffset) {
        return QueryDirectoryNames(path, fileInformationClass, fileNameLengthOffset, fileNameOffset, false, pattern);
    }

    private static string[] QueryDirectoryNames(string path, int fileInformationClass, int fileNameLengthOffset, int fileNameOffset, bool returnSingleEntry) {
        return QueryDirectoryNames(path, fileInformationClass, fileNameLengthOffset, fileNameOffset, returnSingleEntry, null);
    }

    private static string[] QueryDirectoryNames(string path, int fileInformationClass, int fileNameLengthOffset, int fileNameOffset, bool returnSingleEntry, string pattern) {
        using (SafeFileHandle handle = CreateFile(
            path,
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero)) {
            if (handle.IsInvalid) {
                throw new IOException("CreateFile failed: " + Marshal.GetLastWin32Error());
            }

            List<string> names = new List<string>();
            byte[] buffer = new byte[64 * 1024];
            bool restartScan = true;
            IntPtr patternBuffer = IntPtr.Zero;
            IntPtr patternString = IntPtr.Zero;
            try {
                if (!String.IsNullOrEmpty(pattern)) {
                    byte[] patternBytes = System.Text.Encoding.Unicode.GetBytes(pattern);
                    patternBuffer = Marshal.AllocHGlobal(patternBytes.Length + 2);
                    Marshal.Copy(patternBytes, 0, patternBuffer, patternBytes.Length);
                    Marshal.WriteInt16(patternBuffer, patternBytes.Length, 0);
                    UNICODE_STRING unicodePattern = new UNICODE_STRING();
                    unicodePattern.Length = (ushort)patternBytes.Length;
                    unicodePattern.MaximumLength = (ushort)(patternBytes.Length + 2);
                    unicodePattern.Buffer = patternBuffer;
                    patternString = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UNICODE_STRING)));
                    Marshal.StructureToPtr(unicodePattern, patternString, false);
                }

                for (;;) {
                    Array.Clear(buffer, 0, buffer.Length);
                    IO_STATUS_BLOCK iosb;
                    int status = NtQueryDirectoryFile(
                        handle,
                        IntPtr.Zero,
                        IntPtr.Zero,
                        IntPtr.Zero,
                        out iosb,
                        buffer,
                        (uint)buffer.Length,
                        fileInformationClass,
                        returnSingleEntry,
                        patternString,
                        restartScan);
                    restartScan = false;
                    if (status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE) {
                        break;
                    }
                    if (status < 0) {
                        throw new IOException("NtQueryDirectoryFile failed: 0x" + status.ToString("x8"));
                    }

                    int offset = 0;
                    int returned = (int)iosb.Information;
                    while (offset < returned) {
                        int next = BitConverter.ToInt32(buffer, offset);
                        int nameLength = BitConverter.ToInt32(buffer, offset + fileNameLengthOffset);
                        names.Add(System.Text.Encoding.Unicode.GetString(buffer, offset + fileNameOffset, nameLength));
                        if (next == 0) {
                            break;
                        }
                        offset += next;
                    }
                }
            } finally {
                if (patternString != IntPtr.Zero) {
                    Marshal.FreeHGlobal(patternString);
                }
                if (patternBuffer != IntPtr.Zero) {
                    Marshal.FreeHGlobal(patternBuffer);
                }
            }
            names.Sort(StringComparer.OrdinalIgnoreCase);
            return names.ToArray();
        }
    }

    public static void DeleteOnCloseIgnoreReadonly(string path) {
        using (SafeFileHandle handle = CreateFile(
            path,
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            IntPtr.Zero)) {
            if (handle.IsInvalid) {
                throw new IOException("CreateFile failed: " + Marshal.GetLastWin32Error());
            }

            FILE_DISPOSITION_INFO_EX info = new FILE_DISPOSITION_INFO_EX();
            info.Flags = FILE_DISPOSITION_DELETE | FILE_DISPOSITION_ON_CLOSE | FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE;
            if (!SetFileInformationByHandle(handle, FileDispositionInfoEx, ref info, Marshal.SizeOf(typeof(FILE_DISPOSITION_INFO_EX)))) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
        }
    }

    public static void DeleteDirectoryOnCloseIgnoreReadonly(string path) {
        using (SafeFileHandle handle = CreateFile(
            path,
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero)) {
            if (handle.IsInvalid) {
                throw new IOException("CreateFile failed: " + Marshal.GetLastWin32Error());
            }

            FILE_DISPOSITION_INFO_EX info = new FILE_DISPOSITION_INFO_EX();
            info.Flags = FILE_DISPOSITION_DELETE | FILE_DISPOSITION_ON_CLOSE | FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE;
            if (!SetFileInformationByHandle(handle, FileDispositionInfoEx, ref info, Marshal.SizeOf(typeof(FILE_DISPOSITION_INFO_EX)))) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
        }
    }

    public static void RenameReplaceIgnoreReadonly(string sourcePath, string targetPath) {
        using (SafeFileHandle handle = CreateFile(
            sourcePath,
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            IntPtr.Zero)) {
            if (handle.IsInvalid) {
                throw new IOException("CreateFile failed: " + Marshal.GetLastWin32Error());
            }

            byte[] targetBytes = System.Text.Encoding.Unicode.GetBytes(targetPath);
            byte[] info = new byte[20 + targetBytes.Length];
            BitConverter.GetBytes(FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_IGNORE_READONLY_ATTRIBUTE).CopyTo(info, 0);
            BitConverter.GetBytes((long)0).CopyTo(info, 8);
            BitConverter.GetBytes(targetBytes.Length).CopyTo(info, 16);
            Buffer.BlockCopy(targetBytes, 0, info, 20, targetBytes.Length);

            if (!SetFileInformationByHandleBuffer(handle, FileRenameInfoEx, info, info.Length)) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
        }
    }
}
'@
[AgentFsNativeMove]::DeleteOnCloseIgnoreReadonly((Join-Path (Get-Location) 'readonly-delete.txt'))
if (Test-Path readonly-delete.txt) { throw 'FileDispositionInfoEx delete left readonly lower file visible' }
New-Item -ItemType Directory -Force -Path readonly-tree | Out-Null
Set-Content readonly-tree\child.txt 'readonly-tree-child-env'
`$readonlyTreeChild = Get-Item readonly-tree\child.txt
`$readonlyTreeChild.Attributes = `$readonlyTreeChild.Attributes -bor [IO.FileAttributes]::ReadOnly
[AgentFsNativeMove]::DeleteDirectoryOnCloseIgnoreReadonly((Join-Path (Get-Location) 'readonly-tree'))
if (Test-Path readonly-tree) { throw 'FileDispositionInfoEx delete left readonly upper directory tree visible' }
Set-Content readonly-replace-source.txt 'readonly-replace-source-env'
Set-Content readonly-replace-target.txt 'readonly-replace-target-env'
`$readonlyReplaceTarget = Get-Item readonly-replace-target.txt
`$readonlyReplaceTarget.Attributes = `$readonlyReplaceTarget.Attributes -bor [IO.FileAttributes]::ReadOnly
[AgentFsNativeMove]::RenameReplaceIgnoreReadonly(
    (Join-Path (Get-Location) 'readonly-replace-source.txt'),
    (Join-Path (Get-Location) 'readonly-replace-target.txt'))
if (Test-Path readonly-replace-source.txt) { throw 'FileRenameInfoEx replace left readonly replace source visible' }
if ((Get-Content readonly-replace-target.txt) -ne 'readonly-replace-source-env') { throw 'FileRenameInfoEx replace did not update readonly replace target' }
`$crossBoundaryMoveFailed = `$false
if (-not [AgentFsNativeMove]::MoveFileEx('$outsideMoveSource', (Join-Path (Get-Location) 'cross-boundary-move.txt'), 0)) {
    `$crossBoundaryMoveFailed = `$true
}
if (-not `$crossBoundaryMoveFailed) { throw 'external file rename into overlay unexpectedly succeeded' }
if (-not (Test-Path '$outsideMoveSource')) { throw 'failed external rename removed outside source' }
if (Test-Path cross-boundary-move.txt) { throw 'failed external rename created managed target' }
if ([AgentFsNativeMove]::CreateHardLink((Join-Path (Get-Location) 'cross-boundary-link.txt'), '$outsideLinkSource', [IntPtr]::Zero)) {
    throw 'external hardlink into overlay unexpectedly succeeded'
}
if (Test-Path cross-boundary-link.txt) { throw 'failed external hardlink created managed target' }
`$replaceDirFailed = `$false
if (-not [AgentFsNativeMove]::MoveFileEx((Join-Path (Get-Location) 'replace-dir-source.txt'), (Join-Path (Get-Location) 'replace-dir-target'), 1)) {
    `$replaceDirFailed = `$true
}
if (-not `$replaceDirFailed) { throw 'rename replace over existing lower directory unexpectedly succeeded' }
if ((Get-Content replace-dir-source.txt) -ne 'replace-dir-source-original') { throw 'replace-dir failed move hid source file' }
if ((Get-Content replace-dir-target\child.txt) -ne 'replace-dir-target-original') { throw 'replace-dir failed move modified target directory' }
if (-not [AgentFsNativeMove]::MoveFileEx((Join-Path (Get-Location) 'replace-file-source.txt'), (Join-Path (Get-Location) 'replace-file-target.txt'), 1)) {
    throw 'rename replace over existing lower file failed'
}
if (Test-Path replace-file-source.txt) { throw 'replace-file source remained visible after move' }
if ((Get-Content replace-file-target.txt) -ne 'replace-file-source-original') { throw 'replace-file target did not show source content' }
New-Item -ItemType Directory -Force -Path nested\lower | Out-Null
Set-Content nested\lower\deep.txt 'deep-modified'
New-Item -ItemType Directory -Force -Path nested\created\more | Out-Null
Set-Content nested\created\more\new.txt 'new-deep'
Rename-Item nested\created nested\renamed
Set-Content mixed-lower\upper-changed.txt 'mixed-upper-modified'
Rename-Item mixed-lower mixed-renamed
Rename-Item caserename.txt case-renamed.txt
if ((Get-Content case-renamed.txt) -ne 'case-rename-original') { throw 'case-insensitive rename target readback failed' }
if (Test-Path caserename.txt) { throw 'case-insensitive rename source remained visible' }
Rename-Item casedirrename case-dir-renamed
if ((Get-Content case-dir-renamed\child\lower-file.txt) -ne 'case-dir-rename-original') { throw 'case-insensitive directory rename target readback failed' }
if (Test-Path casedirrename) { throw 'case-insensitive directory rename source remained visible' }
Rename-Item move-lower moved-lower
Remove-Item delete-me.txt
Remove-Item casedelete.txt
if (Test-Path casedelete.txt) { throw 'case-insensitive delete left lower file visible' }
Remove-Item -Recurse casedirdelete
if (Test-Path casedirdelete) { throw 'case-insensitive directory delete left lower directory visible' }
Remove-Item -Recurse delete-lower-dir
Remove-Item recreate-me.txt
Set-Content recreate-me.txt 'recreated-in-env'
Remove-Item rename-target.txt
Rename-Item created.txt rename-target.txt
`$names = Get-ChildItem -Name | Sort-Object
if (`$names -notcontains 'host.txt') { throw 'directory listing lost lower file' }
if (`$names -notcontains 'mixed-renamed') { throw 'directory listing lost renamed mixed directory' }
if (`$names -notcontains 'rename-target.txt') { throw 'directory listing lost upper file renamed onto deleted target' }
if (`$names -notcontains 'readonly-replace-target.txt') { throw 'directory listing lost FileRenameInfoEx readonly replace target' }
if (`$names -notcontains 'moved-lower') { throw 'directory listing lost renamed lower directory' }
if (`$names -notcontains 'replace-file-target.txt') { throw 'directory listing lost replaced lower file target' }
if (`$names -notcontains 'recreate-me.txt') { throw 'directory listing lost recreated file' }
if (`$names -notcontains 'case-renamed.txt') { throw 'directory listing lost case-insensitive renamed file' }
if (`$names -notcontains 'case-dir-renamed') { throw 'directory listing lost case-insensitive renamed directory' }
if (`$names -notcontains 'stale-dir') { throw 'directory listing lost recreated upper directory' }
if (`$names -notcontains 'upper-only-dir') { throw 'directory listing lost upper-only directory' }
if (`$names -contains 'delete-me.txt') { throw 'directory listing showed whiteout file' }
if (`$names -contains 'CaseDelete.TXT') { throw 'directory listing showed case-insensitive whiteout file' }
if (`$names -contains 'CaseRename.TXT') { throw 'directory listing showed case-insensitive renamed source' }
if (`$names -contains 'CaseDirDelete') { throw 'directory listing showed case-insensitive whiteouted lower directory' }
if (`$names -contains 'CaseDirRename') { throw 'directory listing showed case-insensitive renamed directory source' }
if (`$names -contains 'delete-lower-dir') { throw 'directory listing showed whiteouted lower directory' }
if (`$names -contains 'lower-symlink.txt') { throw 'directory listing showed whiteouted lower symlink' }
if (`$names -contains 'mixed-lower') { throw 'directory listing showed renamed mixed source' }
if (`$names -contains 'move-lower') { throw 'directory listing showed renamed lower source' }
if (`$names -contains 'readonly-delete.txt') { throw 'directory listing showed readonly disposition-deleted lower file' }
if (`$names -contains 'readonly-tree') { throw 'directory listing showed readonly disposition-deleted upper directory tree' }
if (`$names -contains 'replace-file-source.txt') { throw 'directory listing showed replaced lower file source' }
`$txtNames = Get-ChildItem -Name *.txt | Sort-Object
if (`$txtNames -notcontains 'host.txt') { throw 'wildcard listing lost upper replacement over lower file' }
if (`$txtNames -notcontains 'rename-target.txt') { throw 'wildcard listing lost upper file renamed onto deleted target' }
if (`$txtNames -notcontains 'readonly-replace-target.txt') { throw 'wildcard listing lost FileRenameInfoEx readonly replace target' }
if (`$txtNames -notcontains 'replace-file-target.txt') { throw 'wildcard listing lost replaced lower file target' }
if (`$txtNames -notcontains 'recreate-me.txt') { throw 'wildcard listing lost recreated file' }
if (`$txtNames -notcontains 'case-renamed.txt') { throw 'wildcard listing lost case-insensitive renamed file' }
if (`$txtNames -contains 'delete-me.txt') { throw 'wildcard listing showed whiteouted lower file' }
if (`$txtNames -contains 'CaseDelete.TXT') { throw 'wildcard listing showed case-insensitive whiteouted lower file' }
if (`$txtNames -contains 'CaseRename.TXT') { throw 'wildcard listing showed case-insensitive renamed source' }
if (`$txtNames -contains 'readonly-delete.txt') { throw 'wildcard listing showed readonly disposition-deleted lower file' }
if (`$txtNames -contains 'replace-file-source.txt') { throw 'wildcard listing showed replaced lower file source' }
if (`$txtNames -contains 'lower-symlink.txt') { throw 'wildcard listing showed whiteouted lower symlink' }
`$nativeTxtNames = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, '*.txt', 12, 8, 12)
if (`$nativeTxtNames -notcontains 'host.txt') { throw 'native wildcard listing lost upper replacement over lower file' }
if (`$nativeTxtNames -notcontains 'case-renamed.txt') { throw 'native wildcard listing lost case-insensitive renamed file' }
if (`$nativeTxtNames -notcontains 'replace-file-target.txt') { throw 'native wildcard listing lost replaced lower file target' }
if (`$nativeTxtNames -contains 'delete-me.txt') { throw 'native wildcard listing showed whiteouted lower file' }
if (`$nativeTxtNames -contains 'CaseDelete.TXT') { throw 'native wildcard listing showed case-insensitive whiteouted lower file' }
if (`$nativeTxtNames -contains 'CaseRename.TXT') { throw 'native wildcard listing showed case-insensitive renamed source' }
if (`$nativeTxtNames -contains 'readonly-delete.txt') { throw 'native wildcard listing showed readonly disposition-deleted lower file' }
if (`$nativeTxtNames -contains 'replace-file-source.txt') { throw 'native wildcard listing showed replaced lower file source' }
if (`$nativeTxtNames -contains 'lower-symlink.txt') { throw 'native wildcard listing showed whiteouted lower symlink' }
`$nativeExactHost = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'host.txt', 12, 8, 12)
if (`$nativeExactHost -ne 'host.txt') { throw 'native exact listing lost upper replacement over lower file' }
`$nativeExactCaseRename = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'case-renamed.txt', 12, 8, 12)
if (`$nativeExactCaseRename -ne 'case-renamed.txt') { throw 'native exact listing lost case-insensitive renamed file' }
`$nativeExactRenameTarget = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'rename-target.txt', 12, 8, 12)
if (`$nativeExactRenameTarget -ne 'rename-target.txt') { throw 'native exact listing lost recreated upper file over lower target' }
`$nativeExactDeleted = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'delete-me.txt', 12, 8, 12)
if (`$nativeExactDeleted -contains 'delete-me.txt') { throw 'native exact listing showed whiteouted lower file' }
`$nativeExactCaseRenameSource = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'caserename.txt', 12, 8, 12)
if (`$nativeExactCaseRenameSource -contains 'CaseRename.TXT') { throw 'native exact listing showed case-insensitive renamed source' }
`$nativeExactLowerSymlink = [AgentFsNativeMove]::QueryDirectoryNamesPattern((Get-Location).Path, 'lower-symlink.txt', 12, 8, 12)
if (`$nativeExactLowerSymlink -contains 'lower-symlink.txt') { throw 'native exact listing showed whiteouted lower symlink' }
if ((Get-ChildItem -Name host.txt) -ne 'host.txt') { throw 'exact listing lost upper replacement over lower file' }
if ((Get-ChildItem -Name case-renamed.txt) -ne 'case-renamed.txt') { throw 'exact listing lost case-insensitive renamed file' }
if ((Get-ChildItem -Name case-dir-renamed) -ne 'case-dir-renamed') { throw 'exact listing lost case-insensitive renamed directory' }
if ((Get-ChildItem -Name delete-me.txt -ErrorAction SilentlyContinue) -contains 'delete-me.txt') { throw 'exact listing showed whiteouted lower file' }
if ((Get-ChildItem -Name casedelete.txt -ErrorAction SilentlyContinue) -contains 'CaseDelete.TXT') { throw 'exact listing showed case-insensitive whiteouted lower file' }
if ((Get-ChildItem -Name caserename.txt -ErrorAction SilentlyContinue) -contains 'CaseRename.TXT') { throw 'exact listing showed case-insensitive renamed source' }
if ((Get-ChildItem -Name casedirdelete -ErrorAction SilentlyContinue) -contains 'CaseDirDelete') { throw 'exact listing showed case-insensitive whiteouted lower directory' }
if ((Get-ChildItem -Name casedirrename -ErrorAction SilentlyContinue) -contains 'CaseDirRename') { throw 'exact listing showed case-insensitive renamed directory source' }
if ((Get-ChildItem -Name delete-lower-dir -ErrorAction SilentlyContinue) -contains 'delete-lower-dir') { throw 'exact listing showed whiteouted lower directory' }
if ((Get-ChildItem -Name lower-symlink.txt -ErrorAction SilentlyContinue) -contains 'lower-symlink.txt') { throw 'exact listing showed whiteouted lower symlink' }
if ((Get-ChildItem -Name rename-target.txt) -ne 'rename-target.txt') { throw 'exact listing lost recreated upper file over lower target' }
`$singleNames = [AgentFsNativeMove]::QueryDirectoryNamesSingleEntry((Get-Location).Path, 12, 8, 12)
if (`$singleNames -notcontains 'host.txt') { throw 'single-entry listing lost upper replacement over lower file' }
if (`$singleNames -notcontains 'rename-target.txt') { throw 'single-entry listing lost upper file renamed onto deleted target' }
if (`$singleNames -notcontains 'case-renamed.txt') { throw 'single-entry listing lost case-insensitive renamed file' }
if (`$singleNames -notcontains 'case-dir-renamed') { throw 'single-entry listing lost case-insensitive renamed directory' }
if (`$singleNames -notcontains 'upper-only-dir') { throw 'single-entry listing lost upper-only directory' }
if (`$singleNames -contains 'delete-me.txt') { throw 'single-entry listing showed whiteouted lower file' }
if (`$singleNames -contains 'CaseDelete.TXT') { throw 'single-entry listing showed case-insensitive whiteouted lower file' }
if (`$singleNames -contains 'CaseRename.TXT') { throw 'single-entry listing showed case-insensitive renamed source' }
if (`$singleNames -contains 'CaseDirDelete') { throw 'single-entry listing showed case-insensitive whiteouted lower directory' }
if (`$singleNames -contains 'CaseDirRename') { throw 'single-entry listing showed case-insensitive renamed directory source' }
if (`$singleNames -contains 'delete-lower-dir') { throw 'single-entry listing showed whiteouted lower directory' }
if (`$singleNames -contains 'lower-symlink.txt') { throw 'single-entry listing showed whiteouted lower symlink' }
`$fileIdExtdNames = [AgentFsNativeMove]::QueryDirectoryNames((Get-Location).Path, 60, 60, 88)
if (`$fileIdExtdNames -notcontains 'host.txt') { throw 'FileIdExtdDirectoryInformation lost lower file' }
if (`$fileIdExtdNames -notcontains 'case-renamed.txt') { throw 'FileIdExtdDirectoryInformation lost case-insensitive renamed file' }
if (`$fileIdExtdNames -notcontains 'case-dir-renamed') { throw 'FileIdExtdDirectoryInformation lost case-insensitive renamed directory' }
if (`$fileIdExtdNames -notcontains 'upper-only-dir') { throw 'FileIdExtdDirectoryInformation lost upper directory' }
if (`$fileIdExtdNames -contains 'delete-me.txt') { throw 'FileIdExtdDirectoryInformation showed whiteouted lower file' }
if (`$fileIdExtdNames -contains 'CaseDelete.TXT') { throw 'FileIdExtdDirectoryInformation showed case-insensitive whiteouted lower file' }
if (`$fileIdExtdNames -contains 'CaseRename.TXT') { throw 'FileIdExtdDirectoryInformation showed case-insensitive renamed source' }
if (`$fileIdExtdNames -contains 'CaseDirDelete') { throw 'FileIdExtdDirectoryInformation showed case-insensitive whiteouted lower directory' }
if (`$fileIdExtdNames -contains 'CaseDirRename') { throw 'FileIdExtdDirectoryInformation showed case-insensitive renamed directory source' }
if (`$fileIdExtdNames -contains 'lower-symlink.txt') { throw 'FileIdExtdDirectoryInformation showed whiteouted lower symlink' }
`$fileIdExtdBothNames = [AgentFsNativeMove]::QueryDirectoryNames((Get-Location).Path, 63, 60, 114)
if (`$fileIdExtdBothNames -notcontains 'host.txt') { throw 'FileIdExtdBothDirectoryInformation lost lower file' }
if (`$fileIdExtdBothNames -notcontains 'case-renamed.txt') { throw 'FileIdExtdBothDirectoryInformation lost case-insensitive renamed file' }
if (`$fileIdExtdBothNames -notcontains 'case-dir-renamed') { throw 'FileIdExtdBothDirectoryInformation lost case-insensitive renamed directory' }
if (`$fileIdExtdBothNames -notcontains 'upper-only-dir') { throw 'FileIdExtdBothDirectoryInformation lost upper directory' }
if (`$fileIdExtdBothNames -contains 'delete-me.txt') { throw 'FileIdExtdBothDirectoryInformation showed whiteouted lower file' }
if (`$fileIdExtdBothNames -contains 'CaseDelete.TXT') { throw 'FileIdExtdBothDirectoryInformation showed case-insensitive whiteouted lower file' }
if (`$fileIdExtdBothNames -contains 'CaseRename.TXT') { throw 'FileIdExtdBothDirectoryInformation showed case-insensitive renamed source' }
if (`$fileIdExtdBothNames -contains 'CaseDirDelete') { throw 'FileIdExtdBothDirectoryInformation showed case-insensitive whiteouted lower directory' }
if (`$fileIdExtdBothNames -contains 'CaseDirRename') { throw 'FileIdExtdBothDirectoryInformation showed case-insensitive renamed directory source' }
if (`$fileIdExtdBothNames -contains 'lower-symlink.txt') { throw 'FileIdExtdBothDirectoryInformation showed whiteouted lower symlink' }
`$fileId64ExtdNames = [AgentFsNativeMove]::QueryDirectoryNames((Get-Location).Path, 78, 60, 80)
if (`$fileId64ExtdNames -notcontains 'host.txt') { throw 'FileId64ExtdDirectoryInformation lost lower file' }
if (`$fileId64ExtdNames -notcontains 'case-renamed.txt') { throw 'FileId64ExtdDirectoryInformation lost case-insensitive renamed file' }
if (`$fileId64ExtdNames -notcontains 'case-dir-renamed') { throw 'FileId64ExtdDirectoryInformation lost case-insensitive renamed directory' }
if (`$fileId64ExtdNames -notcontains 'upper-only-dir') { throw 'FileId64ExtdDirectoryInformation lost upper directory' }
if (`$fileId64ExtdNames -contains 'delete-me.txt') { throw 'FileId64ExtdDirectoryInformation showed whiteouted lower file' }
if (`$fileId64ExtdNames -contains 'CaseDelete.TXT') { throw 'FileId64ExtdDirectoryInformation showed case-insensitive whiteouted lower file' }
if (`$fileId64ExtdNames -contains 'CaseRename.TXT') { throw 'FileId64ExtdDirectoryInformation showed case-insensitive renamed source' }
if (`$fileId64ExtdNames -contains 'CaseDirDelete') { throw 'FileId64ExtdDirectoryInformation showed case-insensitive whiteouted lower directory' }
if (`$fileId64ExtdNames -contains 'CaseDirRename') { throw 'FileId64ExtdDirectoryInformation showed case-insensitive renamed directory source' }
if (`$fileId64ExtdNames -contains 'lower-symlink.txt') { throw 'FileId64ExtdDirectoryInformation showed whiteouted lower symlink' }
`$fileId64ExtdBothNames = [AgentFsNativeMove]::QueryDirectoryNames((Get-Location).Path, 79, 60, 106)
if (`$fileId64ExtdBothNames -notcontains 'host.txt') { throw 'FileId64ExtdBothDirectoryInformation lost lower file' }
if (`$fileId64ExtdBothNames -notcontains 'case-renamed.txt') { throw 'FileId64ExtdBothDirectoryInformation lost case-insensitive renamed file' }
if (`$fileId64ExtdBothNames -notcontains 'case-dir-renamed') { throw 'FileId64ExtdBothDirectoryInformation lost case-insensitive renamed directory' }
if (`$fileId64ExtdBothNames -notcontains 'upper-only-dir') { throw 'FileId64ExtdBothDirectoryInformation lost upper directory' }
if (`$fileId64ExtdBothNames -contains 'delete-me.txt') { throw 'FileId64ExtdBothDirectoryInformation showed whiteouted lower file' }
if (`$fileId64ExtdBothNames -contains 'CaseDelete.TXT') { throw 'FileId64ExtdBothDirectoryInformation showed case-insensitive whiteouted lower file' }
if (`$fileId64ExtdBothNames -contains 'CaseRename.TXT') { throw 'FileId64ExtdBothDirectoryInformation showed case-insensitive renamed source' }
if (`$fileId64ExtdBothNames -contains 'CaseDirDelete') { throw 'FileId64ExtdBothDirectoryInformation showed case-insensitive whiteouted lower directory' }
if (`$fileId64ExtdBothNames -contains 'CaseDirRename') { throw 'FileId64ExtdBothDirectoryInformation showed case-insensitive renamed directory source' }
if (`$fileId64ExtdBothNames -contains 'lower-symlink.txt') { throw 'FileId64ExtdBothDirectoryInformation showed whiteouted lower symlink' }
"@

    & $agentctl --agentfs $agentfs session create $EnvId logtest -- powershell.exe -NoProfile -Command "Write-Output 'session-log-stdout'; [Console]::Error.WriteLine('session-log-stderr')"
    $sessionLogText = ""
    for ($i = 0; $i -lt 40; $i++) {
        $sessionLogText = (& $agentctl --agentfs $agentfs session logs $EnvId logtest) -join "`n"
        if ($sessionLogText -match "session-log-stdout" -and $sessionLogText -match "session-log-stderr") {
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if ($sessionLogText -notmatch "session-log-stdout") {
        throw "session stdout was not captured in session logs"
    }
    if ($sessionLogText -notmatch "session-log-stderr") {
        throw "session stderr was not captured in session logs"
    }
    $sessionLogPath = Join-Path $agentfs "envs\$EnvId\logs\sessions\logtest.log"
    if ((Get-Content $sessionLogPath -Raw) -notmatch "session-log-stdout") {
        throw "session stdout was not written to the agentfs log file"
    }
    if ((Get-Content $sessionLogPath -Raw) -notmatch "session-log-stderr") {
        throw "session stderr was not written to the agentfs log file"
    }

    $env:AGENTFS_POST_ENV_HOST_WRITE = Join-Path $source "post-env-host-write.txt"
    & powershell.exe -NoProfile -Command "Set-Content -LiteralPath `$env:AGENTFS_POST_ENV_HOST_WRITE 'post-env-host'; if ((Get-Content -LiteralPath `$env:AGENTFS_POST_ENV_HOST_WRITE) -ne 'post-env-host') { throw 'post-env host process write readback failed' }"
    if ($LASTEXITCODE -ne 0) {
        throw "post-env host process write failed with exit code $LASTEXITCODE"
    }

    $hostContent = Get-Content (Join-Path $source "host.txt")
    if ($hostContent -ne "host-original") {
        throw "host file was modified: $hostContent"
    }
    if ((Get-Content (Join-Path $source "CaseSource.TXT")) -ne "case-original") {
        throw "host CaseSource.TXT was modified"
    }
    if (Test-Path (Join-Path $source "hardlink-host.txt")) {
        throw "host hardlink was created"
    }
    if (Test-Path (Join-Path $source "symlink-host.txt")) {
        throw "host symlink was created"
    }
    if (Test-Path (Join-Path $source "cross-boundary-move.txt")) {
        throw "host cross-boundary move target was created"
    }
    if (Test-Path (Join-Path $source "cross-boundary-link.txt")) {
        throw "host cross-boundary hardlink target was created"
    }
    if (Test-Path (Join-Path $source "readonly-replace-source.txt")) {
        throw "host readonly replace source was created"
    }
    if (Test-Path (Join-Path $source "readonly-tree")) {
        throw "host readonly-tree was created"
    }
    if (Test-Path (Join-Path $source "readonly-replace-target.txt")) {
        throw "host readonly replace target was created"
    }
    if ((Get-Content $outsideMoveSource) -ne "outside-move-original") {
        throw "outside move source was modified"
    }
    if ((Get-Content $outsideLinkSource) -ne "outside-link-original") {
        throw "outside link source was modified"
    }
    if (((Get-Item (Join-Path $source "lower-symlink.txt") -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) {
        throw "host lower symlink stopped being a reparse point"
    }
    if ((Get-Content (Join-Path $source "lower-symlink.txt")) -ne "host-original") {
        throw "host lower symlink target was modified"
    }
    if (-not (Test-Path (Join-Path $source "delete-me.txt"))) {
        throw "host delete-me.txt was removed"
    }
    if ((Get-Content (Join-Path $source "CaseDelete.TXT")) -ne "case-delete-original") {
        throw "host CaseDelete.TXT was modified"
    }
    if ((Get-Content (Join-Path $source "CaseRename.TXT")) -ne "case-rename-original") {
        throw "host CaseRename.TXT was modified"
    }
    if ((Get-Content (Join-Path $source "CaseDirDelete\child\lower-file.txt")) -ne "case-dir-delete-original") {
        throw "host CaseDirDelete tree was modified"
    }
    if ((Get-Content (Join-Path $source "CaseDirRename\child\lower-file.txt")) -ne "case-dir-rename-original") {
        throw "host CaseDirRename tree was modified"
    }
    if ((Get-Content (Join-Path $source "readonly-delete.txt")) -ne "readonly-delete-original") {
        throw "host readonly-delete.txt was modified"
    }
    if (((Get-Item (Join-Path $source "readonly-delete.txt")).Attributes -band [IO.FileAttributes]::ReadOnly) -eq 0) {
        throw "host readonly-delete.txt lost the readonly attribute"
    }
    if ((Get-Content (Join-Path $source "readonly-attributes.txt")) -ne "readonly-attributes-original") {
        throw "host readonly-attributes.txt was modified"
    }
    if (((Get-Item (Join-Path $source "readonly-attributes.txt")).Attributes -band [IO.FileAttributes]::ReadOnly) -eq 0) {
        throw "host readonly-attributes.txt lost the readonly attribute"
    }
    if ((Get-Content (Join-Path $source "delete-lower-dir\child\lower-file.txt")) -ne "delete-lower-dir-original") {
        throw "host delete-lower-dir tree was modified"
    }
    if ((Get-Content (Join-Path $source "recreate-me.txt")) -ne "recreate-original") {
        throw "host recreate-me.txt was modified"
    }
    if ((Get-Content (Join-Path $source "rename-target.txt")) -ne "rename-target-original") {
        throw "host rename-target.txt was modified"
    }
    if (Test-Path (Join-Path $source "child-process.txt")) {
        throw "host child-process.txt was created"
    }
    if (Test-Path (Join-Path $source "grandchild-process.txt")) {
        throw "host grandchild-process.txt was created"
    }
    if ((Get-Content (Join-Path $source "post-env-host-write.txt")) -ne "post-env-host") {
        throw "post-env host process write did not land in source"
    }
    if ((Get-Item (Join-Path $source "metadata.txt")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2019-01-02T03:04:05Z").UtcDateTime) {
        throw "host metadata.txt timestamp was modified"
    }
    if ((Get-Item (Join-Path $source "metadata-dir")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2016-01-02T03:04:05Z").UtcDateTime) {
        throw "host metadata-dir timestamp was modified"
    }
    if ((Get-Content (Join-Path $source "metadata-dir\child.txt")) -ne "metadata-dir-child-original") {
        throw "host metadata-dir child was modified"
    }
    if ([AgentFsEa]::GetDirectoryEa((Join-Path $source "metadata-dir"), "agentfs.metadata.dir.ea") -ne "lower-metadata-dir-ea") {
        throw "host metadata-dir EA was modified"
    }
    if ((Get-Content (Join-Path $source "truncate.txt")) -ne "truncate-original") {
        throw "host truncate.txt was modified"
    }
    if (((Get-Content (Join-Path $source "append.txt")) -join "`n") -ne "append-original") {
        throw "host append.txt was modified"
    }
    if ((Get-Content (Join-Path $source "overwrite.txt")) -ne "overwrite-original") {
        throw "host overwrite.txt was modified"
    }
    if ([IO.File]::ReadAllText((Join-Path $source "open-or-create-existing.txt")) -ne "open-or-create-existing-original") {
        throw "host open-or-create-existing.txt was modified"
    }
    if (Test-Path (Join-Path $source "open-or-create-new.txt")) {
        throw "host open-or-create-new.txt was created"
    }
    if ([IO.File]::ReadAllText((Join-Path $source "create-new-existing.txt")) -ne "create-new-existing-original") {
        throw "host create-new-existing.txt was modified"
    }
    if (Test-Path (Join-Path $source "create-new.txt")) {
        throw "host create-new.txt was created"
    }
    if ((Get-Content (Join-Path $source "mapped.txt")) -ne "0000000000") {
        throw "host mapped.txt was modified"
    }
    if ((Get-Content (Join-Path $source "locked.txt")) -ne "locked-original") {
        throw "host locked.txt was modified"
    }
    if ((Get-Content (Join-Path $source "ea-source.txt")) -ne "ea-main-original") {
        throw "host ea-source.txt was modified"
    }
    if ([AgentFsEa]::GetEa((Join-Path $source "ea-source.txt"), "agentfs.ea") -ne "lower-ea-original") {
        throw "host ea-source.txt EA was modified"
    }
    if ((Get-Content (Join-Path $source "acl-source.txt")) -ne "acl-original") {
        throw "host acl-source.txt was modified"
    }
    if ((Get-Acl (Join-Path $source "acl-source.txt")).Sddl -ne $aclSourceSddl) {
        throw "host acl-source.txt security descriptor was modified"
    }
    if ((Get-Content (Join-Path $source "acl-change-source.txt")) -ne "acl-change-original") {
        throw "host acl-change-source.txt was modified"
    }
    if ((Get-Acl (Join-Path $source "acl-change-source.txt")).Sddl -ne $aclChangeSourceSddl) {
        throw "host acl-change-source.txt security descriptor was modified"
    }
    if ((Get-Content (Join-Path $source "owner-change-source.txt")) -ne "owner-change-original") {
        throw "host owner-change-source.txt was modified"
    }
    if ((Get-Acl (Join-Path $source "owner-change-source.txt")).Sddl -ne $ownerChangeSourceSddl) {
        throw "host owner-change-source.txt security descriptor was modified"
    }
    if ((Get-Content (Join-Path $source "stream-source.txt") -Stream lower) -ne "lower-stream-original") {
        throw "host lower ADS was modified"
    }
    if ((Get-Content (Join-Path $source "stream-source.txt")) -ne "stream-main-original") {
        throw "host stream-source.txt main stream was modified"
    }
    if (Get-Item -Path (Join-Path $source "stream-source.txt") -Stream env -ErrorAction SilentlyContinue) {
        throw "host env ADS was created"
    }
    if ((Get-Content (Join-Path $source "collision-source.txt")) -ne "collision-source-original") {
        throw "host collision-source.txt was modified"
    }
    if ((Get-Content (Join-Path $source "collision-target.txt")) -ne "collision-target-original") {
        throw "host collision-target.txt was modified"
    }
    if ((Get-Content (Join-Path $source "replace-file-source.txt")) -ne "replace-file-source-original") {
        throw "host replace-file-source.txt was modified"
    }
    if ((Get-Content (Join-Path $source "replace-file-target.txt")) -ne "replace-file-target-original") {
        throw "host replace-file-target.txt was modified"
    }
    if ((Get-Content (Join-Path $source "replace-dir-source.txt")) -ne "replace-dir-source-original") {
        throw "host replace-dir-source.txt was modified"
    }
    if ((Get-Content (Join-Path $source "replace-dir-target\child.txt")) -ne "replace-dir-target-original") {
        throw "host replace-dir-target tree was modified"
    }
    if ((Get-Content (Join-Path $source "move-lower\inside\lower-file.txt")) -ne "lower-tree-original") {
        throw "host move-lower tree was modified"
    }
    if ((Get-Item (Join-Path $source "move-lower")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2017-06-05T04:03:02Z").UtcDateTime) {
        throw "host move-lower directory timestamp was modified"
    }
    if ((Get-Acl (Join-Path $source "move-lower")).Sddl -ne $moveLowerSddl) {
        throw "host move-lower directory security descriptor was modified"
    }
    if ([AgentFsEa]::GetDirectoryEa((Join-Path $source "move-lower"), "agentfs.dir.ea") -ne "lower-dir-ea-original") {
        throw "host move-lower directory EA was modified"
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
    if ((Get-Content (Join-Path $upperSource "casesource.txt")) -ne "case-env") {
        throw "case-insensitive lower write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "case-renamed.txt")) -ne "case-rename-original") {
        throw "case-insensitive lower rename was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "casedirdelete")) {
        throw "case-insensitive lower directory delete unexpectedly copied to upper"
    }
    if ((Get-Content (Join-Path $upperSource "case-dir-renamed\child\lower-file.txt")) -ne "case-dir-rename-original") {
        throw "case-insensitive lower directory rename was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "casedirrename")) {
        throw "case-insensitive lower directory rename source remained in upper"
    }
    if ((Get-Content (Join-Path $upperSource "child-process.txt")) -ne "child-env") {
        throw "child process write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "grandchild-process.txt")) -ne "grandchild-env") {
        throw "grandchild process write was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "post-env-host-write.txt")) {
        throw "post-env host process write was redirected to upper"
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
    if ((Get-Item (Join-Path $upperSource "moved-lower")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2017-06-05T04:03:02Z").UtcDateTime) {
        throw "renamed lower directory did not preserve timestamp"
    }
    if ((Get-Acl (Join-Path $upperSource "moved-lower")).Sddl -ne $moveLowerSddl) {
        throw "renamed lower directory did not preserve security descriptor"
    }
    if ([AgentFsEa]::GetDirectoryEa((Join-Path $upperSource "moved-lower"), "agentfs.dir.ea") -ne "lower-dir-ea-original") {
        throw "renamed lower directory did not preserve EA"
    }
    if ((Get-Item (Join-Path $upperSource "moved-lower\inside\lower-file.txt")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2018-07-08T09:10:11Z").UtcDateTime) {
        throw "renamed lower directory tree did not preserve file timestamp"
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
    if (Test-Path (Join-Path $upperSource "readonly-replace-source.txt")) {
        throw "FileRenameInfoEx readonly replace source remained in upper"
    }
    if ((Get-Content (Join-Path $upperSource "readonly-replace-target.txt")) -ne "readonly-replace-source-env") {
        throw "FileRenameInfoEx readonly replace target was not written to upper"
    }
    if (((Get-Item (Join-Path $upperSource "readonly-replace-target.txt")).Attributes -band [IO.FileAttributes]::ReadOnly) -ne 0) {
        throw "FileRenameInfoEx readonly replace target kept readonly attribute"
    }
    if ((Get-Content (Join-Path $upperSource "replace-file-target.txt")) -ne "replace-file-source-original") {
        throw "replaced lower file target was not written to upper"
    }
    if (Test-Path (Join-Path $upperSource "replace-file-source.txt")) {
        throw "replaced lower file source was copied to upper"
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
    if (Test-Path (Join-Path $upperSource "delete-lower-dir")) {
        throw "deleted lower directory was unexpectedly copied to upper"
    }
    if (Test-Path (Join-Path $upperSource "readonly-delete.txt")) {
        throw "readonly disposition-deleted lower file was unexpectedly left in upper"
    }
    if (Test-Path (Join-Path $upperSource "readonly-tree")) {
        throw "readonly disposition-deleted upper directory tree was left in upper"
    }
    if ((Get-Content (Join-Path $upperSource "upper-only-dir\child.txt")) -ne "upper-only-child") {
        throw "upper-only directory child was not written to upper"
    }
    if ((Get-Item (Join-Path $upperSource "metadata.txt")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2020-02-03T04:05:06Z").UtcDateTime) {
        throw "metadata write was not redirected to upper"
    }
    if ((Get-Item (Join-Path $upperSource "metadata-dir")).LastWriteTimeUtc -ne [DateTimeOffset]::Parse("2021-03-04T05:06:07Z").UtcDateTime) {
        throw "directory metadata write was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "metadata-dir\child.txt")) {
        throw "directory metadata write copied lower child into upper"
    }
    if ([AgentFsEa]::GetDirectoryEa((Join-Path $upperSource "metadata-dir"), "agentfs.metadata.dir.ea") -ne "env-dir-ea") {
        throw "directory EA write was not redirected to upper"
    }
    if ([IO.File]::ReadAllText((Join-Path $upperSource "truncate.txt")) -ne "truncate") {
        throw "truncated file was not redirected to upper"
    }
    if (((Get-Content (Join-Path $upperSource "append.txt")) -join "`n") -ne "append-original`nappend-env") {
        throw "append write was not redirected to upper"
    }
    if ([IO.File]::ReadAllText((Join-Path $upperSource "overwrite.txt")) -ne "overwrite-env") {
        throw "overwrite create disposition was not redirected to upper"
    }
    if ([IO.File]::ReadAllText((Join-Path $upperSource "open-or-create-existing.txt")) -ne "open-or-create-existing-env") {
        throw "open-or-create existing lower file was not redirected to upper"
    }
    if ([IO.File]::ReadAllText((Join-Path $upperSource "open-or-create-new.txt")) -ne "open-or-create-new-env") {
        throw "open-or-create new file was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "create-new-existing.txt")) {
        throw "create-new existing lower file unexpectedly created upper"
    }
    if ([IO.File]::ReadAllText((Join-Path $upperSource "create-new.txt")) -ne "create-new-env") {
        throw "create-new file was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "readonly-attributes.txt")) -ne "readonly-attributes-env") {
        throw "readonly attribute write was not redirected to upper"
    }
    if (((Get-Item (Join-Path $upperSource "readonly-attributes.txt")).Attributes -band [IO.FileAttributes]::ReadOnly) -ne 0) {
        throw "readonly attribute clear was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "mapped.txt")) -ne "mapped-env") {
        throw "memory-mapped write was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "locked.txt")) {
        throw "locked lower file was unexpectedly copied to upper"
    }
    if ((Get-Content (Join-Path $upperSource "ea-source.txt")) -ne "ea-main-env") {
        throw "EA source main stream write was not redirected to upper"
    }
    if ([AgentFsEa]::GetEa((Join-Path $upperSource "ea-source.txt"), "agentfs.ea") -ne "env-ea") {
        throw "EA write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "acl-source.txt")) -ne "acl-env") {
        throw "ACL source write was not redirected to upper"
    }
    if ((Get-Acl (Join-Path $upperSource "acl-source.txt")).Sddl -ne $aclSourceSddl) {
        throw "copy-up did not preserve security descriptor"
    }
    if ((Get-Content (Join-Path $upperSource "acl-change-source.txt")) -ne "acl-change-original") {
        throw "ACL-only write did not copy lower content to upper"
    }
    if ((Get-Acl (Join-Path $upperSource "acl-change-source.txt")).Sddl -ne $aclChangeExpectedSddl) {
        throw "ACL-only write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "owner-change-source.txt")) -ne "owner-change-original") {
        throw "owner-only write did not copy lower content to upper"
    }
    if ((Get-Acl (Join-Path $upperSource "owner-change-source.txt")).Sddl -ne $ownerChangeExpectedSddl) {
        throw "owner-only write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "stream-source.txt")) -ne "stream-main-env") {
        throw "ADS source main stream write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "stream-source.txt") -Stream lower) -ne "lower-stream-env") {
        throw "lower ADS write was not redirected to upper"
    }
    if ((Get-Content (Join-Path $upperSource "stream-source.txt") -Stream env) -ne "env-stream") {
        throw "ADS write was not redirected to upper"
    }
    if (Test-Path (Join-Path $upperSource "hardlink-host.txt")) {
        throw "hardlink target was unexpectedly created in upper"
    }
    if (Test-Path (Join-Path $upperSource "cross-boundary-move.txt")) {
        throw "cross-boundary move target was unexpectedly created in upper"
    }
    if (Test-Path (Join-Path $upperSource "cross-boundary-link.txt")) {
        throw "cross-boundary hardlink target was unexpectedly created in upper"
    }
    if (Test-Path (Join-Path $upperSource "lower-symlink.txt")) {
        throw "lower symlink was unexpectedly copied to upper"
    }
    if (Test-Path (Join-Path $upperSource "moved-lower-symlink.txt")) {
        throw "renamed lower symlink was unexpectedly created in upper"
    }
    $upperSymlink = Join-Path $upperSource "symlink-host.txt"
    if ((Test-Path $upperSymlink) -and (((Get-Item $upperSymlink -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "symlink reparse point was unexpectedly created in upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "delete-me.txt"))) {
        throw "delete whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "casedelete.txt"))) {
        throw "case-insensitive delete whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "caserename.txt"))) {
        throw "case-insensitive rename source whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "casedirdelete"))) {
        throw "case-insensitive directory delete whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "casedirrename"))) {
        throw "case-insensitive directory rename source whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "readonly-delete.txt"))) {
        throw "readonly disposition-delete whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "delete-lower-dir"))) {
        throw "lower directory delete whiteout was not created"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "lower-symlink.txt"))) {
        throw "lower symlink delete whiteout was not created"
    }
    if (Test-Path (Join-Path $whiteoutSource "locked.txt")) {
        throw "failed locked lower delete created a whiteout"
    }
    if (Test-Path (Join-Path $whiteoutSource "collision-source.txt")) {
        throw "failed rename collision created a source whiteout"
    }
    if (Test-Path (Join-Path $upperSource "collision-target.txt")) {
        throw "failed rename collision wrote the target to upper"
    }
    if (Test-Path (Join-Path $whiteoutSource "replace-dir-source.txt")) {
        throw "failed replace-dir rename created a source whiteout"
    }
    if (Test-Path (Join-Path $upperSource "replace-dir-target")) {
        throw "failed replace-dir rename wrote the target to upper"
    }
    if (-not (Test-Path (Join-Path $whiteoutSource "replace-file-source.txt"))) {
        throw "replaced lower file source whiteout was not created"
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
    if ($lockedStream) {
        $lockedStream.Dispose()
    }
    if ($daemonProcess -and -not $daemonProcess.HasExited) {
        Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    }
    fltmc unload agentfs 2>$null | Out-Null
    Remove-Item -Recurse -Force $agentfs, $source, $outsideMoveSource, $outsideLinkSource -ErrorAction SilentlyContinue
}
