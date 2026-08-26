# Shipping a release

Everything needed to turn the app into a downloadable installer, and to put that
installer behind a download page.

```
packaging/
    stage_bin.ps1        fetch + verify the ffmpeg/deno/yt-dlp that ship inside
    installer.iss        the Inno Setup script
    build_installer.ps1  build the exe, stage bin\, compile the installer
    publish_release.ps1  upload it to GitHub Releases
    macos/
        Info.plist       bundle metadata, @VERSION@ substituted at build time
        stage_bin.sh     the same fetch + verify, for the arm64 helpers
        build_whispercpp.sh  compile whisper.cpp with Metal, stage it + a model
        build_app.sh     assemble the .app, sign it, produce the .dmg
    staging/, dist/, .cache/    generated, git-ignored
.github/workflows/
    macos.yml            builds the Mac app on a runner (see below)
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

## The macOS build

Building a Mac app needs a Mac, and this project is developed on Windows, so it
happens on a GitHub Actions runner: **Actions → macOS build → Run workflow**, or
automatically on a `v*` tag. The `.dmg` comes back as an artifact.

Locally, on a Mac, the same thing is one command:

```bash
./packaging/macos/build_app.sh
```

Three things about it differ from the Windows story, and all three are
deliberate:

- **Apple Silicon only.** The ffmpeg and deno pinned for macOS are arm64 builds,
  so a universal app would carry helpers half of it could not run. Supporting
  Intel means pinning a second set and lipo-ing the binary.
- **A compiled engine, not a downloaded one.** The Mac transcribes with
  whisper.cpp, because it is the only backend that reaches the GPU here:
  CTranslate2, which Purfview's engine and whisperx both use, has no Metal
  support at all, and the only Mac build Purfview ships is x86-64 and runs under
  Rosetta on the CPU. Measured on an M1 Max, same clip: `small` went from 2.2 to
  17 audio-seconds/s, and `tiny` from 8.9 to 31.

  whisper.cpp publishes no macOS command-line binary — its releases carry
  Windows builds, Linux builds and an Apple xcframework — so there is no
  permanent release asset to pin the way ffmpeg and yt-dlp are.
  `build_whispercpp.sh` compiles it from a pinned tag, which makes the binary
  ours: if it is wrong, it is ours to fix. It checks the result really is arm64,
  because an x86-64 build would still run, under Rosetta, and quietly give up
  the whole point.

  The smallest model ships inside the app (74 MB, in `Contents/Resources/models`)
  so a fresh install can transcribe immediately. The larger ones are downloaded
  on demand through the same pinned-and-checksummed path as every other
  component — see `MODEL_MANIFEST` in `src/download.rs`, which is deliberately
  *one* list rather than one per platform, because a GGML file is identical on
  both.

  Purfview's engine is still the fallback, and the only backend on Windows.
- **Ad-hoc signed, not notarised.** Enough for the binary to launch on arm64,
  which refuses unsigned code outright — but not enough for Gatekeeper, so a
  downloaded copy needs right-click → *Open* the first time. Proper signing
  needs a paid Apple Developer account; with one, swap the ad-hoc `codesign` in
  `build_app.sh` for a Developer ID identity and add a `notarytool` submission.

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
