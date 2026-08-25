<#
.SYNOPSIS
    Stages the helper executables the installer bundles in `bin\`.

.DESCRIPTION
    The app expects ffmpeg, deno and yt-dlp to sit in a `bin\` folder next to
    henrys_shadowing_app.exe (see `bundled_bin_dir` / `resolve_program` in
    src/download.rs). This script fetches exactly the versions pinned in the
    MANIFEST in src/download.rs - it parses that table rather than keeping a
    second copy of the URLs, so the bundled tools can never drift from the ones
    the in-app download manager would fetch.

    Each asset is verified against the SHA-256 pinned in the manifest before it
    is unpacked. A mismatch is fatal: it means the release asset moved, and the
    installer must not ship an unverified binary.

    Downloads are cached in packaging\.cache so re-running is cheap.

.PARAMETER Force
    Re-extract even when packaging\staging\bin already looks complete.

.PARAMETER NoFfprobe
    Skip ffprobe.exe (~90 MB). yt-dlp uses it for probing and degrades to
    warnings without it; leaving it out shrinks the installer.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\stage_bin.ps1
#>
[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$NoFfprobe
)

$ErrorActionPreference = 'Stop'
# Large downloads are an order of magnitude faster with the progress bar off.
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$Packaging = Join-Path $RepoRoot 'packaging'
$CacheDir  = Join-Path $Packaging '.cache'
$StageBin  = Join-Path $Packaging 'staging\bin'
$WorkDir   = Join-Path $CacheDir 'extract'

# ---------------------------------------------------------------------------
# Read the pinned versions out of src/download.rs
# ---------------------------------------------------------------------------

function Get-Manifest {
    $source = Get-Content -Raw (Join-Path $RepoRoot 'src\download.rs')
    $specs = @{}

    foreach ($m in [regex]::Matches($source, '(?s)ToolSpec\s*\{(.*?)\r?\n\s*\},')) {
        $block = $m.Groups[1].Value
        # The `pub struct ToolSpec { .. }` definition matches the same shape;
        # only the MANIFEST entries carry a concrete `id: ToolId::X`.
        if ($block -notmatch 'id:\s*ToolId::(\w+)') { continue }
        $id = $Matches[1]

        $version = $null; $url = $null; $sha = $null
        if ($block -match 'version:\s*"([^"]+)"')            { $version = $Matches[1] }
        if ($block -match 'url:\s*"([^"]+)"')                { $url     = $Matches[1] }
        if ($block -match 'sha256:\s*Some\("([0-9a-fA-F]{64})"\)') { $sha = $Matches[1].ToLower() }

        $specs[$id] = [pscustomobject]@{
            Id = $id; Version = $version; Url = $url; Sha256 = $sha
        }
    }

    foreach ($needed in 'Ffmpeg', 'Deno', 'YtDlp') {
        if (-not $specs.ContainsKey($needed)) {
            throw "could not find $needed in the MANIFEST in src\download.rs"
        }
        if (-not $specs[$needed].Sha256) {
            throw "$needed has no pinned sha256 in src\download.rs - refusing to bundle it"
        }
    }
    return $specs
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLower()
}

function Get-Asset($Spec) {
    $name = [IO.Path]::GetFileName(([Uri]$Spec.Url).AbsolutePath)
    # Version-qualify the cache name: yt-dlp.exe is the same filename in every
    # release, and a stale cache hit would silently ship the wrong build.
    $cached = Join-Path $CacheDir ("{0}-{1}" -f $Spec.Version, $name)

    if ((Test-Path -LiteralPath $cached) -and (Get-Sha256 $cached) -eq $Spec.Sha256) {
        Write-Host ("  cached   {0}" -f (Split-Path -Leaf $cached))
        return $cached
    }

    Write-Host ("  download {0}" -f $Spec.Url)
    $tmp = "$cached.part"
    Invoke-WebRequest -Uri $Spec.Url -OutFile $tmp -UseBasicParsing

    $actual = Get-Sha256 $tmp
    if ($actual -ne $Spec.Sha256) {
        Remove-Item -LiteralPath $tmp -Force
        throw ("SHA-256 mismatch for {0}`n  expected {1}`n  got      {2}`nThe pinned asset changed. Do not ship this." -f $Spec.Id, $Spec.Sha256, $actual)
    }
    Move-Item -LiteralPath $tmp -Destination $cached -Force
    Write-Host ("  verified {0}" -f $Spec.Sha256)
    return $cached
}

function Expand-Asset([string]$Zip, [string]$Into) {
    if (Test-Path -LiteralPath $Into) { Remove-Item -LiteralPath $Into -Recurse -Force }
    New-Item -ItemType Directory -Path $Into -Force | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($Zip, $Into)
}

function Copy-Exe([string]$From, [string]$Name) {
    $found = Get-ChildItem -Path $From -Filter $Name -Recurse -File | Select-Object -First 1
    if (-not $found) { throw "$Name not found under $From" }
    Copy-Item -LiteralPath $found.FullName -Destination (Join-Path $StageBin $Name) -Force
}

# ---------------------------------------------------------------------------

$manifest = Get-Manifest

$wanted = @('ffmpeg.exe', 'deno.exe', 'yt-dlp.exe')
if (-not $NoFfprobe) { $wanted += 'ffprobe.exe' }

$haveAll = $true
foreach ($exe in $wanted) {
    if (-not (Test-Path -LiteralPath (Join-Path $StageBin $exe))) { $haveAll = $false }
}

# What the staging dir holds, recorded when it was filled. Without this, a
# version bump in src\download.rs is invisible here - the exes all exist, so
# staging is skipped and the installer quietly ships the old tool. That is not
# hypothetical: it is how a build "fixing" a stale yt-dlp shipped the stale one.
$stampFile = Join-Path $StageBin 'staged-versions.txt'
$expected = (@('Ffmpeg', 'Deno', 'YtDlp') | ForEach-Object {
    '{0}={1}' -f $_, $manifest[$_].Version
}) -join "`r`n"
$staged = if (Test-Path -LiteralPath $stampFile) {
    (Get-Content -Raw -LiteralPath $stampFile).Trim()
} else {
    ''
}

if ($haveAll -and $staged -eq $expected -and -not $Force) {
    Write-Host "bin\ already staged at the pinned versions (use -Force to rebuild it)" -ForegroundColor Green
} else {
    if ($haveAll -and $staged -ne $expected) {
        Write-Host "pinned versions changed - restaging" -ForegroundColor Yellow
    }
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
    if (Test-Path -LiteralPath $StageBin) { Remove-Item -LiteralPath $StageBin -Recurse -Force }
    New-Item -ItemType Directory -Path $StageBin -Force | Out-Null

    Write-Host ("FFmpeg {0}" -f $manifest['Ffmpeg'].Version)
    $ffmpegZip = Get-Asset $manifest['Ffmpeg']
    $ffmpegDir = Join-Path $WorkDir 'ffmpeg'
    Expand-Asset $ffmpegZip $ffmpegDir
    Copy-Exe $ffmpegDir 'ffmpeg.exe'
    if (-not $NoFfprobe) { Copy-Exe $ffmpegDir 'ffprobe.exe' }

    Write-Host ("Deno {0}" -f $manifest['Deno'].Version)
    $denoZip = Get-Asset $manifest['Deno']
    $denoDir = Join-Path $WorkDir 'deno'
    Expand-Asset $denoZip $denoDir
    Copy-Exe $denoDir 'deno.exe'

    Write-Host ("yt-dlp {0}" -f $manifest['YtDlp'].Version)
    $ytdlp = Get-Asset $manifest['YtDlp']
    Copy-Item -LiteralPath $ytdlp -Destination (Join-Path $StageBin 'yt-dlp.exe') -Force

    if (Test-Path -LiteralPath $WorkDir) { Remove-Item -LiteralPath $WorkDir -Recurse -Force }

    # Written last, so an interrupted staging leaves no stamp and the next run
    # redoes it rather than trusting a half-filled directory.
    Set-Content -Encoding ascii -LiteralPath $stampFile -Value $expected
}

Write-Host ""
Write-Host ("staged in {0}" -f $StageBin)
Get-ChildItem -Path $StageBin -File -Filter '*.exe' | ForEach-Object {
    "{0,-14} {1,8:N1} MB  {2}" -f $_.Name, ($_.Length / 1MB), (Get-Sha256 $_.FullName)
}
