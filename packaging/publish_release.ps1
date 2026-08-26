<#
.SYNOPSIS
    Publishes a built installer as a GitHub release, which is what the
    download page on GitHub Pages points at.

.DESCRIPTION
    Tags the current commit v<version>, creates the release, and uploads the
    installer together with its .sha256. The download page reads the newest
    release through the GitHub API and points its button at the first asset
    whose name looks like *Setup*.exe, so nothing here needs to be repeated on
    the website when a new version goes out.

    Run packaging\build_installer.ps1 first; this script only publishes what is
    already in packaging\dist.

    Needs the GitHub CLI, signed in:  gh auth login

.PARAMETER Version
    Version to publish. Defaults to the `version` in Cargo.toml.

.PARAMETER Notes
    Extra text placed at the top of the release notes.

.PARAMETER Draft
    Create the release as a draft, to look it over before it goes public.

.PARAMETER Force
    Publish even with uncommitted changes in the working tree, without asking.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\publish_release.ps1
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Notes,
    [switch]$Draft,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$Packaging = Join-Path $RepoRoot 'packaging'

if (-not (Get-Command 'gh' -ErrorAction SilentlyContinue)) {
    throw "GitHub CLI not found. Install it from https://cli.github.com and run 'gh auth login'."
}

if (-not $Version) {
    $cargo = Get-Content -Raw (Join-Path $RepoRoot 'Cargo.toml')
    if ($cargo -match '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw 'could not read version from Cargo.toml - pass -Version instead'
    }
}

$tag   = "v$Version"
$setup = Join-Path $Packaging "dist\HenrysShadowingApp-Setup-$Version.exe"
$sha   = "$setup.sha256"

foreach ($f in $setup, $sha) {
    if (-not (Test-Path -LiteralPath $f)) {
        throw "missing $f - run build_installer.ps1 -Version $Version first"
    }
}

# A release is a snapshot of a commit; publishing one built from uncommitted
# work would tag something nobody else can reproduce.
Push-Location $RepoRoot
try {
    $dirty = & git status --porcelain
    if ($dirty -and -not $Force) {
        Write-Warning "working tree has uncommitted changes:"
        $dirty | ForEach-Object { Write-Warning "  $_" }
        $answer = Read-Host "Publish $tag anyway? (y/N)"
        if ($answer -ne 'y') { throw 'aborted' }
    }

    # Ask for the list rather than viewing the one tag: `gh release view` on a
    # tag that does not exist writes to stderr, and Windows PowerShell turns a
    # redirected native stderr line into a terminating error - which would look
    # like a failure when it is in fact the ordinary case.
    if (& git tag --list $tag) {
        throw "tag $tag already exists locally. Bump the version in Cargo.toml, or remove it with: git tag -d $tag"
    }
    $published = & gh release list --limit 100 --json tagName --jq '.[].tagName'
    if ($published -contains $tag) {
        throw "release $tag already exists. Bump the version in Cargo.toml, or delete it with: gh release delete $tag"
    }
} finally {
    Pop-Location
}

# Pin versions straight out of the manifest, so the notes always describe what
# actually shipped rather than what someone remembered to type. Only the Windows
# list: src\download.rs carries one MANIFEST per target_os now, and this script
# publishes the Windows installer.
$downloadRs = Get-Content -Raw (Join-Path $RepoRoot 'src\download.rs')
$windowsManifest = '(?s)#\[cfg\((?<cfg>[^\]]*)\)\]\s*\r?\npub const MANIFEST[^=]*=\s*&\[(?<body>.*?)\r?\n\];'
$downloadRs = ([regex]::Matches($downloadRs, $windowsManifest) |
    Where-Object { $_.Groups['cfg'].Value -match 'target_os = "windows"' } |
    ForEach-Object { $_.Groups['body'].Value } |
    Select-Object -First 1)
if (-not $downloadRs) { throw 'no Windows MANIFEST found in src\download.rs' }

$bundled = New-Object System.Collections.Generic.List[string]
foreach ($m in [regex]::Matches($downloadRs, '(?s)ToolSpec\s*\{(.*?)\r?\n\s*\},')) {
    $block = $m.Groups[1].Value
    if ($block -notmatch 'display_name:\s*"([^"]+)"') { continue }
    $name = $Matches[1]
    if ($block -match 'version:\s*"([^"]+)"') {
        $bundled.Add(("- {0} {1}" -f $name, $Matches[1]))
    }
}

# Note on the doubled backticks below: this is a double-quoted here-string, so
# PowerShell reads a backtick as its escape character. `` is one literal
# backtick, which is what markdown wants around inline code, and `````` is a
# fenced block.
$hash = (Get-Content -Raw $sha).Split(' ')[0].Trim()
$sizeMb = '{0:N0}' -f ((Get-Item -LiteralPath $setup).Length / 1MB)

$body = @"
$Notes

**[Download HenrysShadowingApp-Setup-$Version.exe](https://github.com/henry4324234/henrys_shadowing_app/releases/download/$tag/HenrysShadowingApp-Setup-$Version.exe)** - $sizeMb MB, Windows 10/11 64-bit.

Installs for the current user only, so it needs no administrator rights. The installer is not code-signed: SmartScreen will ask, choose **More info** then **Run anyway**.

On first run the app downloads the Faster-Whisper transcription engine (~1.4 GB) into ``%LOCALAPPDATA%\henrys_shadowing_app``.

### Components

Bundled in ``bin\`` next to the app, or fetched by it on demand:

$($bundled -join "`n")

### Checksum

``````
SHA-256  $hash
``````

Verify with ``Get-FileHash .\HenrysShadowingApp-Setup-$Version.exe``.
"@

$ghArgs = @('release', 'create', $tag, $setup, $sha,
            '--title', "Henry's Shadowing App $Version",
            '--notes', $body)
if ($Draft) { $ghArgs += '--draft' }

Write-Host "publishing $tag ..." -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & gh @ghArgs
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Host ""
Write-Host "released: https://github.com/henry4324234/henrys_shadowing_app/releases/tag/$tag" -ForegroundColor Green
Write-Host "download page: https://henry4324234.github.io/henrys_shadowing_app/"
Write-Host "(the page picks up the new version by itself - nothing to edit)"
