<#
.SYNOPSIS
    Build and package SoundHop for Windows (Portable ZIP + Inno Setup Installer + SHA256SUMS.txt).

.DESCRIPTION
    Builds the release workspace binaries (soundhop.exe and soundhop-gui.exe), stages the minimal
    required GStreamer plugin DLLs under lib\gstreamer-1.0\ plus all transitive GStreamer/GLib/Opus/SRTP
    runtime DLLs from the MSVC x86_64 GStreamer installation, smoke-tests the staged CLI with a
    stripped PATH, packages a self-contained portable ZIP archive, optionally invokes Inno Setup 6
    (iscc.exe) to build the Windows installer executable, and writes SHA256SUMS.txt.

.PARAMETER Version
    Override the package version. Defaults to [workspace.package] version in Cargo.toml.

.PARAMETER OutDir
    Output directory for release artifacts. Defaults to "dist" under the repository root.

.PARAMETER TargetDir
    Cargo release target directory containing soundhop.exe and soundhop-gui.exe.
    Defaults to "target\release" under the repository root.

.PARAMETER GStreamerRoot
    Root directory of the GStreamer MSVC x86_64 installation.
    Defaults to $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 or "C:\gstreamer\1.0\msvc_x86_64".

.PARAMETER SkipBuild
    Skip running `cargo build --release --workspace --locked` and package existing binaries.

.PARAMETER SkipSmokeTest
    Skip running the stripped-PATH self-contained smoke check against the staged binaries.

.PARAMETER RequireInstaller
    Fail with a non-zero exit code if Inno Setup 6 (iscc.exe) is not installed.
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$OutDir = "",
    [string]$TargetDir = "",
    [string]$GStreamerRoot = "",
    [switch]$SkipBuild,
    [switch]$SkipSmokeTest,
    [switch]$RequireInstaller
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "..\..")).Path

$RequiredGstPlugins = @(
    "gstcoreelements.dll",
    "gstaudioconvert.dll",
    "gstaudioresample.dll",
    "gstvolume.dll",
    "gstopus.dll",
    "gstrtp.dll",
    "gstrtpmanager.dll",
    "gstsrtp.dll",
    "gstudp.dll",
    "gstwasapi2.dll",
    "gstapp.dll"
)

function Get-WorkspaceVersion {
    param([string]$CargoTomlPath)

    $inWorkspacePackage = $false
    foreach ($line in (Get-Content -LiteralPath $CargoTomlPath)) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^\[.+\]$') {
            $inWorkspacePackage = ($trimmed -eq '[workspace.package]')
            continue
        }
        if ($inWorkspacePackage -and $trimmed -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "Could not determine [workspace.package] version from $CargoTomlPath"
}

function Find-Iscc {
    $cmd = Get-Command iscc -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        return $cmd.Source
    }

    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
    return $null
}

function Resolve-GStreamerRoot {
    param([string]$ExplicitRoot)

    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($ExplicitRoot)) {
        $candidates += $ExplicitRoot
    }
    if (-not [string]::IsNullOrWhiteSpace($env:GSTREAMER_1_0_ROOT_MSVC_X86_64)) {
        $candidates += $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
    }
    $candidates += "C:\gstreamer\1.0\msvc_x86_64"

    foreach ($candidate in $candidates) {
        $trimmed = $candidate.TrimEnd('\', '/')
        if ((Test-Path -LiteralPath (Join-Path $trimmed "bin")) -and
            (Test-Path -LiteralPath (Join-Path $trimmed "lib\gstreamer-1.0"))) {
            return (Resolve-Path -LiteralPath $trimmed).Path
        }
    }

    throw "Could not locate GStreamer MSVC x86_64 installation (checked: $($candidates -join ', ')). Install GStreamer MSVC x86_64 runtime+devel or pass -GStreamerRoot."
}

function Read-NullTerminatedAscii {
    param(
        [byte[]]$Bytes,
        [int]$Offset
    )

    if ($Offset -lt 0 -or $Offset -ge $Bytes.Length) {
        return $null
    }
    $end = $Offset
    while ($end -lt $Bytes.Length -and $Bytes[$end] -ne 0) {
        $end++
    }
    if ($end -eq $Offset) {
        return ""
    }
    return [System.Text.Encoding]::ASCII.GetString($Bytes, $Offset, $end - $Offset)
}

function Convert-RvaToFileOffset {
    param(
        [uint32]$Rva,
        [object[]]$Sections
    )

    $rva64 = [uint64]$Rva
    foreach ($sec in $Sections) {
        $va64 = [uint64]$sec.VirtualAddress
        $rawSize64 = [uint64]$sec.SizeOfRawData
        $span = [Math]::Max([uint64]$sec.VirtualSize, $rawSize64)
        if ($rva64 -ge $va64 -and $rva64 -lt ($va64 + $span)) {
            $delta = $rva64 - $va64
            if ($delta -lt $rawSize64) {
                return [int]([uint64]$sec.PointerToRawData + $delta)
            }
        }
    }
    return -1
}

function Get-PeImportedDlls {
    param([string]$FilePath)

    $bytes = [System.IO.File]::ReadAllBytes($FilePath)
    if ($bytes.Length -lt 0x40) {
        return ,[string[]]@()
    }
    if ([BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) {
        return ,[string[]]@()
    }

    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -lt 0 -or ($peOffset + 24) -gt $bytes.Length) {
        return ,[string[]]@()
    }
    if ([BitConverter]::ToUInt32($bytes, $peOffset) -ne 0x00004550) {
        return ,[string[]]@()
    }

    $numSections = [BitConverter]::ToUInt16($bytes, $peOffset + 6)
    $optHeaderSize = [BitConverter]::ToUInt16($bytes, $peOffset + 20)
    $optHeaderOffset = $peOffset + 24
    if (($optHeaderOffset + $optHeaderSize) -gt $bytes.Length) {
        return ,[string[]]@()
    }

    $magic = [BitConverter]::ToUInt16($bytes, $optHeaderOffset)
    if ($magic -eq 0x20B) {
        # PE32+ (x86_64)
        $dataDirOffset = $optHeaderOffset + 112
        $numRvaAndSizes = [BitConverter]::ToUInt32($bytes, $optHeaderOffset + 108)
    } elseif ($magic -eq 0x10B) {
        # PE32 (x86)
        $dataDirOffset = $optHeaderOffset + 96
        $numRvaAndSizes = [BitConverter]::ToUInt32($bytes, $optHeaderOffset + 92)
    } else {
        return ,[string[]]@()
    }

    $sections = New-Object System.Collections.Generic.List[object]
    $secTableOffset = $optHeaderOffset + $optHeaderSize
    for ($i = 0; $i -lt $numSections; $i++) {
        $off = $secTableOffset + ($i * 40)
        if (($off + 40) -gt $bytes.Length) {
            break
        }
        $sections.Add([PSCustomObject]@{
            VirtualSize      = [BitConverter]::ToUInt32($bytes, $off + 8)
            VirtualAddress   = [BitConverter]::ToUInt32($bytes, $off + 12)
            SizeOfRawData    = [BitConverter]::ToUInt32($bytes, $off + 16)
            PointerToRawData = [BitConverter]::ToUInt32($bytes, $off + 20)
        })
    }
    $sectionArray = $sections.ToArray()
    $dllNames = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)

    # Standard Import Directory (DataDirectory[1])
    if ($numRvaAndSizes -gt 1) {
        $importRva = [BitConverter]::ToUInt32($bytes, $dataDirOffset + 8)
        if ($importRva -ne 0) {
            $descOff = Convert-RvaToFileOffset -Rva $importRva -Sections $sectionArray
            while ($descOff -ge 0 -and ($descOff + 20) -le $bytes.Length) {
                $origFirstThunk = [BitConverter]::ToUInt32($bytes, $descOff)
                $nameRva = [BitConverter]::ToUInt32($bytes, $descOff + 12)
                $firstThunk = [BitConverter]::ToUInt32($bytes, $descOff + 16)
                if ($origFirstThunk -eq 0 -and $nameRva -eq 0 -and $firstThunk -eq 0) {
                    break
                }
                if ($nameRva -ne 0) {
                    $nameOff = Convert-RvaToFileOffset -Rva $nameRva -Sections $sectionArray
                    $dllName = Read-NullTerminatedAscii -Bytes $bytes -Offset $nameOff
                    if (-not [string]::IsNullOrWhiteSpace($dllName)) {
                        [void]$dllNames.Add($dllName)
                    }
                }
                $descOff += 20
            }
        }
    }

    # Delay Import Directory (DataDirectory[13])
    if ($numRvaAndSizes -gt 13) {
        $delayRva = [BitConverter]::ToUInt32($bytes, $dataDirOffset + (13 * 8))
        if ($delayRva -ne 0) {
            $descOff = Convert-RvaToFileOffset -Rva $delayRva -Sections $sectionArray
            while ($descOff -ge 0 -and ($descOff + 32) -le $bytes.Length) {
                $attrs = [BitConverter]::ToUInt32($bytes, $descOff)
                $nameRva = [BitConverter]::ToUInt32($bytes, $descOff + 4)
                if ($attrs -eq 0 -and $nameRva -eq 0) {
                    break
                }
                # Modern MSVC uses RVA-based delay import descriptors (bit 0 of grAttrs set)
                if (($attrs -band 1) -ne 0 -and $nameRva -ne 0) {
                    $nameOff = Convert-RvaToFileOffset -Rva $nameRva -Sections $sectionArray
                    $dllName = Read-NullTerminatedAscii -Bytes $bytes -Offset $nameOff
                    if (-not [string]::IsNullOrWhiteSpace($dllName)) {
                        [void]$dllNames.Add($dllName)
                    }
                }
                $descOff += 32
            }
        }
    }

    $result = [System.Collections.Generic.List[string]]::new($dllNames)
    return ,$result.ToArray()
}

function Stage-GStreamerRuntime {
    param(
        [string]$GstRoot,
        [string]$StagingDir,
        [string[]]$SeedBinaries,
        [string[]]$PluginDllNames
    )

    $gstBinDir = Join-Path $GstRoot "bin"
    $gstPluginDir = Join-Path $GstRoot "lib\gstreamer-1.0"
    $stagedPluginDir = Join-Path $StagingDir "lib\gstreamer-1.0"
    New-Item -ItemType Directory -Force -Path $stagedPluginDir | Out-Null

    $scanQueue = [System.Collections.Generic.Queue[string]]::new()
    $visited = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)

    foreach ($binPath in $SeedBinaries) {
        if ($visited.Add($binPath)) {
            $scanQueue.Enqueue($binPath)
        }
    }

    # 1. Stage minimal required GStreamer plugins under lib\gstreamer-1.0\
    Write-Host "==> Staging $($PluginDllNames.Count) required GStreamer plugin DLLs..." -ForegroundColor Cyan
    foreach ($pluginName in $PluginDllNames) {
        $srcPlugin = Join-Path $gstPluginDir $pluginName
        if (-not (Test-Path -LiteralPath $srcPlugin)) {
            throw "Required GStreamer plugin DLL not found: $srcPlugin"
        }
        $dstPlugin = Join-Path $stagedPluginDir $pluginName
        Copy-Item -LiteralPath $srcPlugin -Destination $dstPlugin -Force
        if ($visited.Add($dstPlugin)) {
            $scanQueue.Enqueue($dstPlugin)
        }
    }

    # 2. Transitively resolve and copy dependent runtime DLLs from GStreamer's bin\ directory
    $stagedRuntimeDlls = [System.Collections.Generic.List[string]]::new()
    while ($scanQueue.Count -gt 0) {
        $currentBinary = $scanQueue.Dequeue()
        foreach ($dllName in (Get-PeImportedDlls -FilePath $currentBinary)) {
            if ([string]::IsNullOrWhiteSpace($dllName)) {
                continue
            }
            $candidate = Join-Path $gstBinDir $dllName
            if (-not (Test-Path -LiteralPath $candidate)) {
                continue
            }
            $dstDll = Join-Path $StagingDir (Split-Path -Leaf $candidate)
            if ($visited.Add($dstDll)) {
                Copy-Item -LiteralPath $candidate -Destination $dstDll -Force
                $stagedRuntimeDlls.Add((Split-Path -Leaf $candidate))
                $scanQueue.Enqueue($dstDll)
            }
        }
    }

    if ($stagedRuntimeDlls.Count -eq 0) {
        throw "No GStreamer runtime DLLs were resolved from $gstBinDir; check PE imports and GStreamer installation."
    }

    Write-Host "    Staged $($PluginDllNames.Count) plugin DLLs and $($stagedRuntimeDlls.Count) transitive runtime DLLs from $gstBinDir"
}

function Invoke-StrippedPathSmokeTest {
    param(
        [string]$StagingDir,
        [string]$TempRegistryPath
    )

    $stagedCli = Join-Path $StagingDir "soundhop.exe"
    Write-Host "==> Smoke-testing staged soundhop.exe with stripped PATH (no system GStreamer)..." -ForegroundColor Cyan

    $envNames = @(
        "PATH",
        "GSTREAMER_1_0_ROOT_MSVC_X86_64",
        "GST_PLUGIN_PATH",
        "GST_PLUGIN_PATH_1_0",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SYSTEM_PATH_1_0",
        "GST_REGISTRY_1_0",
        "GST_REGISTRY_FORK"
    )
    $savedEnv = @{}
    foreach ($name in $envNames) {
        $savedEnv[$name] = [System.Environment]::GetEnvironmentVariable($name, "Process")
    }

    try {
        $env:PATH = "$env:SystemRoot\System32;$env:SystemRoot"
        foreach ($clearName in @(
            "GSTREAMER_1_0_ROOT_MSVC_X86_64",
            "GST_PLUGIN_PATH",
            "GST_PLUGIN_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SYSTEM_PATH_1_0"
        )) {
            [System.Environment]::SetEnvironmentVariable($clearName, $null, "Process")
        }
        $env:GST_REGISTRY_1_0 = $TempRegistryPath

        $versionOut = & $stagedCli --version
        if ($LASTEXITCODE -ne 0) {
            throw "Staged soundhop.exe --version failed under stripped PATH with exit code $LASTEXITCODE"
        }
        Write-Host "    $versionOut"

        # Run `soundhop doctor` to exercise bundled GStreamer plugin loading;
        # tolerate audio/firewall warnings on headless CI runners, but fail on DLL load errors.
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $doctorOut = & $stagedCli doctor 2>&1
            $doctorExit = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($doctorExit -eq -1073741515 -or $doctorExit -eq -1073741511) {
            throw "Staged soundhop.exe doctor failed with Windows DLL loader error ($doctorExit): $doctorOut"
        }
        Write-Host "    doctor exit code: $doctorExit"
    } finally {
        foreach ($name in $envNames) {
            [System.Environment]::SetEnvironmentVariable($name, $savedEnv[$name], "Process")
        }
        if (Test-Path -LiteralPath $TempRegistryPath) {
            Remove-Item -LiteralPath $TempRegistryPath -Force -ErrorAction SilentlyContinue
        }
    }
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = Get-WorkspaceVersion -CargoTomlPath (Join-Path $RepoRoot "Cargo.toml")
}

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $RepoRoot "dist"
} elseif (-not [System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir = Join-Path $RepoRoot $OutDir
}

if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    $TargetDir = Join-Path $RepoRoot "target\release"
} elseif (-not [System.IO.Path]::IsPathRooted($TargetDir)) {
    $TargetDir = Join-Path $RepoRoot $TargetDir
}

$ResolvedGstRoot = Resolve-GStreamerRoot -ExplicitRoot $GStreamerRoot
Write-Host "==> Packaging SoundHop v$Version for Windows (x86_64)" -ForegroundColor Cyan
Write-Host "    Using GStreamer root: $ResolvedGstRoot"

if (-not $SkipBuild) {
    Write-Host "==> Building release binaries (soundhop + soundhop-gui)..." -ForegroundColor Cyan
    Push-Location $RepoRoot
    try {
        & cargo build --release --workspace --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

$CliExe = Join-Path $TargetDir "soundhop.exe"
$GuiExe = Join-Path $TargetDir "soundhop-gui.exe"
$LicenseFile = Join-Path $RepoRoot "LICENSE"
$ReadmeFile = Join-Path $RepoRoot "README.md"
$IssFile = Join-Path $ScriptDir "soundhop.iss"

foreach ($required in @($CliExe, $GuiExe, $LicenseFile, $ReadmeFile, $IssFile)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Required file not found: $required"
    }
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$TargetDir = (Resolve-Path -LiteralPath $TargetDir).Path

# 1. Stage binaries, docs, and minimal GStreamer runtime + plugins
$PortableName = "SoundHop-$Version-windows-x86_64-portable"
$StagingRoot = Join-Path $OutDir "_staging_portable"
$StagingDir = Join-Path $StagingRoot $PortableName
$PortableZip = Join-Path $OutDir "$PortableName.zip"

if (Test-Path -LiteralPath $StagingRoot) {
    Remove-Item -LiteralPath $StagingRoot -Recurse -Force
}
if (Test-Path -LiteralPath $PortableZip) {
    Remove-Item -LiteralPath $PortableZip -Force
}

$Artifacts = [System.Collections.Generic.List[string]]::new()

try {
    New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null
    $StagedGuiExe = Join-Path $StagingDir "soundhop-gui.exe"
    $StagedCliExe = Join-Path $StagingDir "soundhop.exe"
    Copy-Item -LiteralPath $GuiExe -Destination $StagedGuiExe
    Copy-Item -LiteralPath $CliExe -Destination $StagedCliExe
    Copy-Item -LiteralPath $LicenseFile -Destination (Join-Path $StagingDir "LICENSE")
    Copy-Item -LiteralPath $ReadmeFile -Destination (Join-Path $StagingDir "README.md")

    Stage-GStreamerRuntime `
        -GstRoot $ResolvedGstRoot `
        -StagingDir $StagingDir `
        -SeedBinaries @($StagedCliExe, $StagedGuiExe) `
        -PluginDllNames $RequiredGstPlugins

    if (-not $SkipSmokeTest) {
        Invoke-StrippedPathSmokeTest `
            -StagingDir $StagingDir `
            -TempRegistryPath (Join-Path $StagingRoot "smoke-gst-registry.bin")
    }

    # 2. Build Portable ZIP
    Write-Host "==> Creating portable archive: $PortableZip" -ForegroundColor Cyan
    Compress-Archive -Path $StagingDir -DestinationPath $PortableZip -CompressionLevel Optimal -Force
    $Artifacts.Add($PortableZip)

    # 3. Build Inno Setup Installer from the staged directory (if iscc is available)
    $IsccPath = Find-Iscc
    $InstallerExe = Join-Path $OutDir "SoundHop-$Version-windows-x86_64-setup.exe"

    if ($null -ne $IsccPath) {
        Write-Host "==> Building Inno Setup installer with $IsccPath..." -ForegroundColor Cyan
        if (Test-Path -LiteralPath $InstallerExe) {
            Remove-Item -LiteralPath $InstallerExe -Force
        }
        & $IsccPath `
            "/DBuildDir=$TargetDir" `
            "/DStageDir=$StagingDir" `
            "/DOutputDirPath=$OutDir" `
            $IssFile
        if ($LASTEXITCODE -ne 0) {
            throw "Inno Setup compiler (iscc) failed with exit code $LASTEXITCODE"
        }
        if (-not (Test-Path -LiteralPath $InstallerExe)) {
            throw "Expected installer was not created: $InstallerExe"
        }
        $Artifacts.Add($InstallerExe)
    } elseif ($RequireInstaller) {
        throw "Inno Setup 6 (iscc.exe) was not found, and -RequireInstaller was specified."
    } else {
        Write-Warning "Inno Setup 6 (iscc.exe) not found; skipping installer .exe build (portable ZIP was created)."
    }
} finally {
    if (Test-Path -LiteralPath $StagingRoot) {
        Remove-Item -LiteralPath $StagingRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# 4. Generate SHA256SUMS.txt
$ChecksumFile = Join-Path $OutDir "SHA256SUMS.txt"
$ChecksumLines = foreach ($artifactPath in $Artifacts) {
    $hash = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $fileName = Split-Path -Leaf $artifactPath
    "$hash  $fileName"
}
$ChecksumContent = ($ChecksumLines -join "`n") + "`n"
[System.IO.File]::WriteAllText($ChecksumFile, $ChecksumContent, [System.Text.UTF8Encoding]::new($false))

Write-Host "==> Generated $ChecksumFile" -ForegroundColor Cyan
Get-Content -LiteralPath $ChecksumFile | ForEach-Object { Write-Host "    $_" }
Write-Host "==> Done." -ForegroundColor Green
