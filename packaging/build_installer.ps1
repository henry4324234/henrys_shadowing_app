<#
.SYNOPSIS
    Builds HenrysShadowingApp-Setup-<version>.exe end to end.

.DESCRIPTION
    Three steps, each skippable:
      1. cargo build --release
      2. stage_bin.ps1  - fetch + verify the ffmpeg/deno/yt-dlp the installer
                          bundles in bin\
      3. ISCC           - compile packaging\installer.iss

    The result lands in packaging\dist together with a .sha256 file, which is
    what the download page publishes so people can check what they downloaded.

.PARAMETER Version
    Version stamped into the installer and its filename. Defaults to the
    `version` in Cargo.toml.

.PARAMETER SkipBuild
    Use the target\release exe that is already there.

.PARAMETER SkipStage
    Use packaging\staging\bin as it already is.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\build_installer.ps1
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipBuild,
    [switch]$SkipStage
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$Packaging = Join-Path $RepoRoot 'packaging'
$DistDir   = Join-Path $Packaging 'dist'

function Find-Iscc {
    $cmd = Get-Command 'iscc.exe' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    $candidates = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($c in $candidates) {
        if (Test-Path -LiteralPath $c) { return $c }
    }
    throw @"
Inno Setup 6 not found. Install it with:
    winget install -e --id JRSoftware.InnoSetup
or set the path to ISCC.exe on PATH.
"@
}

if (-not $Version) {
    $cargo = Get-Content -Raw (Join-Path $RepoRoot 'Cargo.toml')
    # The first `version = "..."` under [package], before any dependency has
    # a chance to introduce its own.
    if ($cargo -match '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw 'could not read version from Cargo.toml - pass -Version instead'
    }
}
Write-Host "Henry's Shadowing App $Version" -ForegroundColor Cyan

if (-not $SkipBuild) {
    Write-Host "`n[1/3] cargo build --release" -ForegroundColor Cyan
    Push-Location $RepoRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
} else {
    Write-Host "`n[1/3] cargo build - skipped" -ForegroundColor DarkGray
}

$appExe = Join-Path $RepoRoot 'target\release\henrys_shadowing_app.exe'
if (-not (Test-Path -LiteralPath $appExe)) {
    throw "missing $appExe - run without -SkipBuild"
}

if (-not $SkipStage) {
    Write-Host "`n[2/3] staging bin\" -ForegroundColor Cyan
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $Packaging 'stage_bin.ps1')
    if ($LASTEXITCODE -ne 0) { throw "staging failed ($LASTEXITCODE)" }
} else {
    Write-Host "`n[2/3] staging - skipped" -ForegroundColor DarkGray
}

foreach ($exe in 'ffmpeg.exe', 'deno.exe', 'yt-dlp.exe') {
    $p = Join-Path $Packaging "staging\bin\$exe"
    if (-not (Test-Path -LiteralPath $p)) { throw "missing $p - run without -SkipStage" }
}

Write-Host "`n[3/3] compiling the installer" -ForegroundColor Cyan
$iscc = Find-Iscc
New-Item -ItemType Directory -Path $DistDir -Force | Out-Null

& $iscc "/DAppVersion=$Version" (Join-Path $Packaging 'installer.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

$setup = Join-Path $DistDir "HenrysShadowingApp-Setup-$Version.exe"
if (-not (Test-Path -LiteralPath $setup)) { throw "ISCC reported success but $setup is missing" }

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $setup).Hash.ToLower()
Set-Content -Encoding ascii -Path "$setup.sha256" -Value ("{0}  {1}" -f $hash, (Split-Path -Leaf $setup))

$sizeMb = (Get-Item -LiteralPath $setup).Length / 1MB
Write-Host ""
Write-Host ("{0}" -f $setup) -ForegroundColor Green
Write-Host ("{0:N1} MB" -f $sizeMb)
Write-Host ("sha256 {0}" -f $hash)
Write-Host ""
Write-Host "Publish it with: packaging\publish_release.ps1 -Version $Version"
