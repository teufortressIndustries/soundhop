<#
.SYNOPSIS
    Build and package SoundHop for Windows (Portable ZIP + Inno Setup Installer + SHA256SUMS.txt).

.DESCRIPTION
    Builds the release workspace binaries (soundhop.exe and soundhop-gui.exe), smoke-tests the CLI,
    packages a portable ZIP archive, optionally invokes Inno Setup 6 (iscc.exe) to build the
    Windows installer executable, and writes SHA256SUMS.txt in the output directory.

.PARAMETER Version
    Override the package version. Defaults to [workspace.package] version in Cargo.toml.

.PARAMETER OutDir
    Output directory for release artifacts. Defaults to "dist" under the repository root.

.PARAMETER TargetDir
    Cargo release target directory containing soundhop.exe and soundhop-gui.exe.
    Defaults to "target\release" under the repository root.

.PARAMETER SkipBuild
    Skip running `cargo build --release --workspace --locked` and package existing binaries.

.PARAMETER RequireInstaller
    Fail with a non-zero exit code if Inno Setup 6 (iscc.exe) is not installed.
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$OutDir = "",
    [string]$TargetDir = "",
    [switch]$SkipBuild,
    [switch]$RequireInstaller
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "..\..")).Path

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

Write-Host "==> Packaging SoundHop v$Version for Windows (x86_64)" -ForegroundColor Cyan

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

Write-Host "==> Smoke-testing soundhop.exe --version..." -ForegroundColor Cyan
$cliVersionOut = & $CliExe --version
if ($LASTEXITCODE -ne 0) {
    throw "soundhop.exe --version failed with exit code $LASTEXITCODE"
}
Write-Host "    $cliVersionOut"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$TargetDir = (Resolve-Path -LiteralPath $TargetDir).Path

# 1. Stage and build Portable ZIP
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

New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null
Copy-Item -LiteralPath $GuiExe -Destination (Join-Path $StagingDir "soundhop-gui.exe")
Copy-Item -LiteralPath $CliExe -Destination (Join-Path $StagingDir "soundhop.exe")
Copy-Item -LiteralPath $LicenseFile -Destination (Join-Path $StagingDir "LICENSE")
Copy-Item -LiteralPath $ReadmeFile -Destination (Join-Path $StagingDir "README.md")

Write-Host "==> Creating portable archive: $PortableZip" -ForegroundColor Cyan
Compress-Archive -Path $StagingDir -DestinationPath $PortableZip -CompressionLevel Optimal -Force
Remove-Item -LiteralPath $StagingRoot -Recurse -Force

$Artifacts = [System.Collections.Generic.List[string]]::new()
$Artifacts.Add($PortableZip)

# 2. Build Inno Setup Installer (if iscc is available)
$IsccPath = Find-Iscc
$InstallerExe = Join-Path $OutDir "SoundHop-$Version-windows-x86_64-setup.exe"

if ($null -ne $IsccPath) {
    Write-Host "==> Building Inno Setup installer with $IsccPath..." -ForegroundColor Cyan
    if (Test-Path -LiteralPath $InstallerExe) {
        Remove-Item -LiteralPath $InstallerExe -Force
    }
    & $IsccPath `
        "/DBuildDir=$TargetDir" `
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

# 3. Generate SHA256SUMS.txt
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
