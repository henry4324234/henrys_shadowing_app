# Shipping a release

Everything needed to turn the app into a downloadable Windows installer, and to
put that installer behind a download page.

```
packaging/
    stage_bin.ps1        fetch + verify the ffmpeg/deno/yt-dlp that ship inside
    installer.iss        the Inno Setup script
    build_installer.ps1  build the exe, stage bin\, compile the installer
    publish_release.ps1  upload it to GitHub Releases
    staging/, dist/, .cache/    generated, git-ignored
docs/
    index.html           the download page (GitHub Pages)
```

## Once, per machine

- **Rust** — the toolchain that already builds the app.
- **Inno Setup 6** — `winget install -e --id JRSoftware.InnoSetup`
- **GitHub CLI**, signed in — `winget install -e --id GitHub.cli` then `gh auth login`
  (only needed to publish).

## Building the installer

```powershell
powershell -ExecutionPolicy Bypass -File packaging\build_installer.ps1
```

That runs `cargo build --release`, stages `bin\`, and compiles
`packaging\dist\HenrysShadowingApp-Setup-<version>.exe` plus a `.sha256`
alongside it. First run downloads about 220 MB of helper tools into
`packaging\.cache`; later runs reuse them, so they take about half a minute.

`-SkipBuild` and `-SkipStage` skip the first two steps when only the installer
script has changed. The version comes from `Cargo.toml` unless `-Version` says
otherwise.

### What the installer does

- Installs **per user**, into `%LOCALAPPDATA%\Programs\Henrys Shadowing App`, so
  it never asks for administrator rights.
- Puts `ffmpeg.exe`, `ffprobe.exe`, `deno.exe` and `yt-dlp.exe` in `bin\` next to
  the app, which is the first place `resolve_program` (in `src/download.rs`)
  looks. Nothing is added to `PATH`.
- Adds a Start Menu entry, and a desktop icon if asked.
- Upgrades in place: the `AppId` GUID in `installer.iss` is what ties versions
  together, so leave it alone forever.
- On uninstall, offers to delete the downloaded transcription engine in
  `%LOCALAPPDATA%\henrys_shadowing_app` and the settings in
  `%APPDATA%\henrys_shadowing_app`, defaulting to keeping both.

The multi-gigabyte transcription engine is deliberately *not* in the installer —
the app downloads it on first run, with a progress bar and a resumable-enough
retry, which is a far better experience than a 1.5 GB setup file.

## Publishing it

```powershell
powershell -ExecutionPolicy Bypass -File packaging\publish_release.ps1
```

Tags the current commit `v<version>`, creates the GitHub release, uploads the
installer and its checksum, and writes release notes that list the pinned
component versions read out of `src/download.rs`. Use `-Draft` to look it over
before it goes public.

The download page needs no edit for a new version: it asks the GitHub API for
the newest release and points its button at the `*Setup*.exe` asset. Without
JavaScript, or if the API is rate-limited, the button falls back to the releases
page.

### Turning the page on, once

GitHub repo → **Settings → Pages → Build and deployment → Deploy from a branch**,
branch `master`, folder `/docs`. It then serves at
<https://henry4324234.github.io/henrys_shadowing_app/>.

## Bumping a bundled tool

`MANIFEST` in `src/download.rs` is the single source of truth: the app's own
downloader and `stage_bin.ps1` both read it, so they cannot drift apart.

1. Update `version`, `url` and `sha256` together for that tool.
2. `powershell -File packaging\stage_bin.ps1 -Force` — it fails loudly if the
   asset does not match the pinned hash.
3. Rebuild the installer.

Pin permanent, versioned release assets. FFmpeg is pinned to Gyan's releases
rather than BtbN's autobuilds for exactly this reason: BtbN deletes old
autobuild tags after a few weeks, and a 404 there breaks every already-shipped
copy of the app.

## Code signing

The installer is unsigned, so SmartScreen shows "Windows protected your PC" on
a freshly published version until enough people click through. With an
Authenticode certificate, uncomment the two `SignTool` lines in `installer.iss`
and define the tool once in the Inno Setup IDE (Tools → Configure Sign Tools),
or pass `/Ssigntool=...` to ISCC.
