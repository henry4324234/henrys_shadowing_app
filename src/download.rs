//! Managed-tool download manager.
//!
//! Downloads pinned versions of external tools (ffmpeg, deno, yt-dlp,
//! Faster-Whisper) into a per-user managed directory, verifies them against
//! a hardcoded SHA-256, extracts them, and exposes lookup helpers so
//! `deps.rs` / `pipeline.rs` can prefer managed installs over PATH.
//!
//! Layout on disk (Windows):
//!
//! ```text
//! %LOCALAPPDATA%\henrys_shadowing_app\
//!     downloads\                     in-flight *.part files (resumable-ish:
//!                                    deleted and restarted on next attempt)
//!     tools\
//!         yt-dlp-2026.07.04\         one dir per tool+version
//!             yt-dlp.exe
//!             installed.ok           marker: written last, checked first
//!         faster-whisper-r245.4\
//!             Faster-Whisper-XXL\...
//!             installed.ok
//! ```
//!
//! The installer additionally ships ffmpeg/deno/yt-dlp in a `bin\` folder
//! next to the app exe; [`resolve_program`] checks that first, then the
//! managed dir, then falls back to the bare name (PATH).
//!
//! Threading model matches the rest of the app: no async runtime. Call
//! [`spawn_install`] from the UI thread; it runs on a worker thread and
//! reports [`DownloadMsg`]s over a crossbeam channel. Cancellation is a
//! shared `AtomicBool` checked between reads.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ToolId {
    Ffmpeg,
    Deno,
    YtDlp,
    FasterWhisper,
    /// One whisper.cpp GGML model. Unlike the tools, these are per-accuracy
    /// and platform-neutral - the same file works on either OS - so they live
    /// in `MODEL_MANIFEST` rather than in the per-target lists below.
    WhisperModel(crate::pipeline::WhisperModel),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayloadKind {
    /// Asset is the executable itself; no extraction step.
    RawExe,
    /// Asset is a .zip; extracted, then `exe_name` is located by walking.
    Zip,
    /// Asset is a .7z; extracted, then `exe_name` is located by walking.
    SevenZ,
}

pub struct ToolSpec {
    pub id: ToolId,
    pub display_name: &'static str,
    /// Version label; also used to name the install dir, so bumping the
    /// pinned version installs side-by-side rather than in-place.
    pub version: &'static str,
    /// Pinned release asset. Never point this at a `latest` tag — a moving
    /// URL silently breaks the SHA-256 pin below.
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the asset. `None` skips verification (only
    /// acceptable while wiring things up; ship with a real hash).
    pub sha256: Option<&'static str>,
    pub kind: PayloadKind,
    /// File to locate inside the extracted tree (or the name to give a
    /// `RawExe`). Windows name including `.exe`.
    pub exe_name: &'static str,
    /// Approximate download size in bytes, for UI before headers arrive.
    pub approx_size: u64,
}

/// Pinned tool versions. Windows x64 assets — the download manager is part
/// of the Windows shipping story; macOS/Linux users install via package
/// managers and are found through the existing PATH/pipx detection.
///
/// To bump a version: update `version`, `url`, `sha256` together.
/// `stage_bin.ps1` prints the SHA-256 of everything it downloads, or use
/// `certutil -hashfile <file> SHA256`. ffmpeg/deno/yt-dlp publish checksum
/// assets alongside each release; verify against those, not just a local
/// hash of whatever was downloaded.
/// Windows assets. Also used on Linux and the BSDs, which aren't a shipping
/// target - there's no installer for them and the tools come from the distro -
/// but keeping a full list there means [`spec`] stays total, and nothing
/// downloads it: the UI only ever offers the engine, and a Linux user is served
/// by the PATH/pipx detection instead.
#[cfg(any(target_os = "windows", not(target_os = "macos")))]
pub const MANIFEST: &[ToolSpec] = &[
    // Gyan's release builds rather than BtbN's autobuilds: BtbN deletes old
    // autobuild tags after a few weeks, which 404s the pinned URL (and with
    // it every install of a shipped version). Gyan tags releases by ffmpeg
    // version and the assets stay put. The essentials build carries what the
    // pipeline asks for — libx264, h264_nvenc/amf/qsv, aac.
    ToolSpec {
        id: ToolId::Ffmpeg,
        display_name: "FFmpeg",
        version: "9.0.1-essentials",
        url: "https://github.com/GyanD/codexffmpeg/releases/download/9.0.1/ffmpeg-9.0.1-essentials_build.zip",
        sha256: Some("fec81ae03971d9dd4be3ebe02e263bd2ec1d789483f931bdba5f5715e65da2e9"),
        kind: PayloadKind::Zip,
        exe_name: "ffmpeg.exe",
        approx_size: 111_253_802,
    },
    ToolSpec {
        id: ToolId::Deno,
        display_name: "Deno",
        version: "2.9.2",
        url: "https://github.com/denoland/deno/releases/download/v2.9.2/deno-x86_64-pc-windows-msvc.zip",
        sha256: Some("5fe194d26ac5ef77fcc5288c2c438c7a0465f3b6180440ebf04092714bf2dcdf"),
        kind: PayloadKind::Zip,
        exe_name: "deno.exe",
        approx_size: 45 * 1024 * 1024,
    },
    ToolSpec {
        id: ToolId::YtDlp,
        display_name: "yt-dlp",
        // Keep this one close to current. YouTube rotates whatever the
        // extractor has to defeat, and a yt-dlp that is a few weeks old starts
        // taking `HTTP Error 403: Forbidden` on the media URL for more and
        // more videos while still working on others - which reads as "your app
        // is broken", not "this tool is stale". Verified against the
        // SHA2-256SUMS published with the release.
        version: "2026.08.19",
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp.exe",
        sha256: Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a"),
        kind: PayloadKind::RawExe,
        exe_name: "yt-dlp.exe",
        approx_size: 17_840_399,
    },
    ToolSpec {
        id: ToolId::FasterWhisper,
        display_name: "Faster-Whisper (transcription engine)",
        version: "r245.4",
        url: "https://github.com/Purfview/whisper-standalone-win/releases/download/Faster-Whisper-XXL/Faster-Whisper-XXL_r245.4_windows.7z",
        sha256: Some("237dee23939cdabfc96ef859fc5e584b842c3a5557e0d2ca744e1f87c14c5844"),
        kind: PayloadKind::SevenZ,
        exe_name: "faster-whisper-xxl.exe",
        approx_size: 1_424_256_246,
    },
];

/// macOS assets, Apple Silicon. The helper tools are arm64 builds, so this is
/// not a universal app: an Intel Mac would download binaries it cannot run.
///
/// The engine is the awkward one. Purfview publishes no Apple Silicon build and
/// no XXL build for macOS at all - the newest Mac asset is Whisper-Faster
/// r186.1, x86-64, from 2024. It runs under Rosetta 2 and only on the CPU (no
/// CUDA on a Mac), which is slower than the Windows path but keeps the
/// one-click install; a system whisperx is still preferred when present, and is
/// the faster option on Apple Silicon because it can reach the GPU through MPS.
#[cfg(target_os = "macos")]
pub const MANIFEST: &[ToolSpec] = &[
    ToolSpec {
        id: ToolId::Ffmpeg,
        display_name: "FFmpeg",
        version: "6.0-static-arm64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.0/ffmpeg-darwin-arm64",
        sha256: Some("a90e3db6a3fd35f6074b013f948b1aa45b31c6375489d39e572bea3f18336584"),
        kind: PayloadKind::RawExe,
        exe_name: "ffmpeg",
        approx_size: 45_568_216,
    },
    ToolSpec {
        id: ToolId::Deno,
        display_name: "Deno",
        version: "2.9.2",
        url: "https://github.com/denoland/deno/releases/download/v2.9.2/deno-aarch64-apple-darwin.zip",
        sha256: Some("687ae485168ba73a4f1ee3a954eb4f077eca82f2fefd236a6a83a3889287876c"),
        kind: PayloadKind::Zip,
        exe_name: "deno",
        approx_size: 37_981_362,
    },
    ToolSpec {
        id: ToolId::YtDlp,
        display_name: "yt-dlp",
        // The macOS asset is a universal binary, so this one is fine on Intel.
        version: "2026.08.19",
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_macos",
        sha256: Some("0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202"),
        kind: PayloadKind::RawExe,
        exe_name: "yt-dlp",
        approx_size: 37_146_048,
    },
    ToolSpec {
        id: ToolId::FasterWhisper,
        display_name: "Faster-Whisper (transcription engine)",
        version: "r186.1-macos-x86-64",
        url: "https://github.com/Purfview/whisper-standalone-win/releases/download/faster-whisper/Whisper-Faster_r186.1_macOS-x86-64.zip",
        sha256: Some("863e9d41cd889bfd5417bec5d8d48f04d0a0e6c97b6f45c7910a85e08798a3bc"),
        kind: PayloadKind::Zip,
        exe_name: "whisper-faster",
        approx_size: 82_515_821,
    },
];

/// The whisper.cpp models, one per accuracy setting.
///
/// Deliberately *not* inside the per-`target_os` MANIFESTs above. A GGML file
/// is the same bytes on either platform, so duplicating these per OS would be
/// five entries to keep in step for no benefit - and the packaging scripts that
/// read `src/download.rs` walk the per-target lists, which would then have
/// counted every model twice.
///
/// The smallest ships inside the app (see `bundled_model_dir`), and is listed
/// here anyway so an install that somehow lacks it can still fetch it.
///
/// Hashes are the LFS object ids from the HuggingFace API, which are plain
/// SHA-256 of the file - verified against a locally computed hash for tiny.
pub const MODEL_MANIFEST: &[ToolSpec] = &[
    ToolSpec {
        id: ToolId::WhisperModel(crate::pipeline::WhisperModel::Tiny),
        display_name: "Transcription model (Lowest Accuracy)",
        version: "ggml-tiny",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        sha256: Some("be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21"),
        kind: PayloadKind::RawExe,
        exe_name: "ggml-tiny.bin",
        approx_size: 77_691_713,
    },
    ToolSpec {
        id: ToolId::WhisperModel(crate::pipeline::WhisperModel::Base),
        display_name: "Transcription model (Low Accuracy)",
        version: "ggml-base",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha256: Some("60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe"),
        kind: PayloadKind::RawExe,
        exe_name: "ggml-base.bin",
        approx_size: 147_951_465,
    },
    ToolSpec {
        id: ToolId::WhisperModel(crate::pipeline::WhisperModel::Small),
        display_name: "Transcription model (Medium Accuracy)",
        version: "ggml-small",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha256: Some("1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"),
        kind: PayloadKind::RawExe,
        exe_name: "ggml-small.bin",
        approx_size: 487_601_967,
    },
    ToolSpec {
        id: ToolId::WhisperModel(crate::pipeline::WhisperModel::Medium),
        display_name: "Transcription model (High Accuracy)",
        version: "ggml-medium",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha256: Some("6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208"),
        kind: PayloadKind::RawExe,
        exe_name: "ggml-medium.bin",
        approx_size: 1_533_763_059,
    },
    ToolSpec {
        id: ToolId::WhisperModel(crate::pipeline::WhisperModel::LargeV3),
        display_name: "Transcription model (Highest Accuracy)",
        version: "ggml-large-v3",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
        sha256: Some("64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2"),
        kind: PayloadKind::RawExe,
        exe_name: "ggml-large-v3.bin",
        approx_size: 3_095_033_483,
    },
];

pub fn spec(id: ToolId) -> &'static ToolSpec {
    MANIFEST
        .iter()
        .chain(MODEL_MANIFEST.iter())
        .find(|s| s.id == id)
        .expect("every ToolId has a manifest entry")
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// Per-user root for managed downloads. Deliberately *not* the install
/// directory (Program Files may be read-only) and *not* the settings dir
/// (%APPDATA% roams on domain machines; a 2 GB tool tree shouldn't).
pub fn managed_root() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("henrys_shadowing_app"));
    }

    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("henrys_shadowing_app")
                .join("managed")
        });
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        return std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".local")
                .join("share")
                .join("henrys_shadowing_app")
        });
    }
}

/// Where the installer puts the ffmpeg/deno/yt-dlp it ships with.
///
/// Next to the executable on Windows. On macOS the executable lives inside the
/// bundle at `Contents/MacOS/`, and anything that isn't code belongs one floor
/// up in `Contents/Resources/` - putting the tools beside the binary would
/// work, but it breaks the convention codesign and notarisation expect.
pub fn bundled_bin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    #[cfg(target_os = "macos")]
    {
        if dir.ends_with("Contents/MacOS") {
            return Some(dir.parent()?.join("Resources").join("bin"));
        }
    }

    Some(dir.join("bin"))
}

/// Where the transcription model that ships *inside* the app lives.
///
/// Only the smallest model rides along - it is 74 MB, which an installer can
/// carry without becoming a nuisance, and it means a fresh install can
/// transcribe something the moment it opens, with nothing to download first.
/// Larger models are fetched on demand into `managed_root()/models`.
pub fn bundled_model_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    #[cfg(target_os = "macos")]
    {
        if dir.ends_with("Contents/MacOS") {
            return Some(dir.parent()?.join("Resources").join("models"));
        }
    }

    Some(dir.join("models"))
}

fn tool_dir(spec: &ToolSpec) -> Option<PathBuf> {
    let dir_name = match spec.id {
        ToolId::Ffmpeg => format!("ffmpeg-{}", spec.version),
        ToolId::Deno => format!("deno-{}", spec.version),
        ToolId::YtDlp => format!("yt-dlp-{}", spec.version),
        ToolId::FasterWhisper => format!("faster-whisper-{}", spec.version),
        // Models sit under models/ rather than tools/: they are data, and
        // keeping them apart means the engine and its models can be cleared
        // independently.
        ToolId::WhisperModel(_) => {
            return Some(managed_root()?.join("models").join(spec.version));
        }
    };
    Some(managed_root()?.join("tools").join(dir_name))
}

const MARKER: &str = "installed.ok";

/// Path to a tool's executable if the pinned version is fully installed in
/// the managed dir. Checks the marker file first: a dir without a marker is
/// a broken/interrupted install and is ignored (the next install attempt
/// clears and redoes it).
pub fn installed_exe(id: ToolId) -> Option<PathBuf> {
    let spec = spec(id);
    let dir = tool_dir(spec)?;
    if !dir.join(MARKER).is_file() {
        return None;
    }
    find_file(&dir, spec.exe_name)
}

/// Resolve a plain program name ("ffmpeg", "deno", "yt-dlp") for spawning:
/// installer-bundled `bin\` first, then the managed download dir, then the
/// bare name so `Command` falls back to PATH. Intended as a drop-in inside
/// `no_window_command(...)` call sites:
///
/// ```ignore
/// no_window_command(download::resolve_program("ffmpeg"))
/// ```
pub fn resolve_program(name: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    if let Some(bin) = bundled_bin_dir() {
        let candidate = bin.join(&exe);
        if candidate.is_file() {
            return candidate;
        }
    }

    let id = match name {
        "ffmpeg" => Some(ToolId::Ffmpeg),
        "deno" => Some(ToolId::Deno),
        "yt-dlp" => Some(ToolId::YtDlp),
        _ => None,
    };
    if let Some(id) = id {
        if let Some(path) = installed_exe(id) {
            return path;
        }
    }

    PathBuf::from(name)
}

/// Depth-capped walk for a file called `name` under `root`, so a
/// pathological archive can't send us searching forever.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut queue: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if path
                    .file_name()
                    .map(|n| n.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
                {
                    return Some(path);
                }
            } else if path.is_dir() && depth < 6 {
                queue.push((path, depth + 1));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Install worker
// ---------------------------------------------------------------------------

/// Progress messages sent from the worker thread. Poll alongside `JobMsg`
/// in `App::update` and call `ctx.request_repaint()` while a download is
/// live so the progress bar animates.
#[derive(Debug)]
pub enum DownloadMsg {
    /// Bytes downloaded so far, and the total if the server reported one.
    Progress(ToolId, u64, Option<u64>),
    /// Download finished; hash check running (fast, but visible on 1.4 GB).
    Verifying(ToolId),
    /// Archive being unpacked — the slow step for the whisper build, so it
    /// reports bytes written and the expanded total rather than just spinning.
    /// A total of 0 means "unknown", and the UI falls back to a spinner.
    Extracting(ToolId, u64, u64),
    /// Fully installed; path is the tool's executable. The UI doesn't need
    /// the path (it re-runs the dependency check instead), but pipeline-side
    /// consumers of this module do — hence the allow.
    Done(ToolId, #[allow(dead_code)] PathBuf),
    /// User hit cancel; partial files have been cleaned up.
    Cancelled(ToolId),
    Failed(ToolId, String),
}

/// Download + verify + extract `spec` on a worker thread.
///
/// `cancel` is checked between network reads; set it to true and the worker
/// stops within one read, deletes its partial files, and sends `Cancelled`.
pub fn spawn_install(
    id: ToolId,
    tx: Sender<DownloadMsg>,
    cancel: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let spec = spec(id);
        match install(spec, &tx, &cancel) {
            Ok(exe) => {
                let _ = tx.send(DownloadMsg::Done(id, exe));
            }
            Err(InstallError::Cancelled) => {
                let _ = tx.send(DownloadMsg::Cancelled(id));
            }
            Err(InstallError::Other(msg)) => {
                let _ = tx.send(DownloadMsg::Failed(id, msg));
            }
        }
    })
}

enum InstallError {
    Cancelled,
    Other(String),
}

impl From<String> for InstallError {
    fn from(s: String) -> Self {
        InstallError::Other(s)
    }
}

fn install(
    spec: &ToolSpec,
    tx: &Sender<DownloadMsg>,
    cancel: &AtomicBool,
) -> Result<PathBuf, InstallError> {
    let root = managed_root()
        .ok_or_else(|| "cannot determine app data directory".to_string())?;
    let final_dir = tool_dir(spec).expect("root resolved above");

    // Already installed (e.g. two clicks racing, or a re-check).
    if final_dir.join(MARKER).is_file() {
        if let Some(exe) = find_file(&final_dir, spec.exe_name) {
            return Ok(exe);
        }
        // Marker without exe: something deleted files underneath us.
        // Fall through and reinstall.
        let _ = fs::remove_dir_all(&final_dir);
    }

    let downloads = root.join("downloads");
    fs::create_dir_all(&downloads)
        .map_err(|e| format!("cannot create {}: {e}", downloads.display()))?;

    let file_name = spec
        .url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    let part_path = downloads.join(format!("{file_name}.part"));

    // --- Download, hashing as we go -------------------------------------
    let digest_hex = download_to(spec, &part_path, tx, cancel).map_err(|e| {
        let _ = fs::remove_file(&part_path);
        e
    })?;

    // --- Verify ----------------------------------------------------------
    let _ = tx.send(DownloadMsg::Verifying(spec.id));
    match spec.sha256 {
        Some(expected) => {
            let expected = expected.to_ascii_lowercase();
            if digest_hex != expected {
                let _ = fs::remove_file(&part_path);
                return Err(format!(
                    "{}: SHA-256 mismatch — expected {expected}, got {digest_hex}. \
                     The download may be corrupt or the release replaced; \
                     not installing.",
                    spec.display_name
                )
                .into());
            }
        }
        None => {
            // Deliberate policy decision surfaced loudly in debug builds.
            debug_assert!(false, "shipping a tool without a pinned SHA-256");
        }
    }

    // --- Extract into a staging dir, then rename into place --------------
    let _ = tx.send(DownloadMsg::Extracting(spec.id, 0, 0));
    let staging = final_dir.with_extension("partial");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;

    let unpack_result: Result<(), InstallError> = match spec.kind {
        PayloadKind::RawExe => {
            let dest = staging.join(spec.exe_name);
            fs::copy(&part_path, &dest)
                .map(|_| ())
                .map_err(|e| InstallError::Other(format!("copy failed: {e}")))
        }
        PayloadKind::Zip => extract_zip(&part_path, &staging).map_err(InstallError::Other),
        PayloadKind::SevenZ => extract_7z(&part_path, &staging, spec.id, tx, cancel),
    };
    let _ = fs::remove_file(&part_path);
    if let Err(e) = unpack_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(match e {
            InstallError::Cancelled => InstallError::Cancelled,
            InstallError::Other(msg) => {
                InstallError::Other(format!("{}: {msg}", spec.display_name))
            }
        });
    }

    let exe_in_staging = find_file(&staging, spec.exe_name).ok_or_else(|| {
        let _ = fs::remove_dir_all(&staging);
        format!(
            "{}: extracted archive does not contain {} — \
             the release layout may have changed",
            spec.display_name, spec.exe_name
        )
    })?;

    // A raw download has no permission bits to inherit, and a zip's are lost
    // unless the archive carried them — either way the file comes out
    // non-executable, and spawning it fails with a bare "permission denied"
    // that looks like a broken install. Windows has no such notion.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&exe_in_staging, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "{}: cannot make {} executable: {e}",
                spec.display_name, spec.exe_name
            )
            .into());
        }
    }

    drop(exe_in_staging); // path becomes stale after the rename below

    // Marker goes in before the rename: after the rename the directory is
    // complete-by-construction, and a crash before it leaves only a
    // `.partial` dir that the next attempt wipes.
    let marker_body = format!(
        "version={}\nurl={}\nsha256={}\n",
        spec.version,
        spec.url,
        spec.sha256.unwrap_or("(unverified)")
    );
    fs::write(staging.join(MARKER), marker_body)
        .map_err(|e| format!("cannot write marker: {e}"))?;

    if let Some(parent) = final_dir.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_dir_all(&final_dir);
    fs::rename(&staging, &final_dir).map_err(|e| {
        format!(
            "cannot move {} into place: {e}",
            final_dir.display()
        )
    })?;

    find_file(&final_dir, spec.exe_name)
        .ok_or_else(|| "installed but executable not found".to_string().into())
}

/// Stream `spec.url` to `dest`, returning the lowercase hex SHA-256 of the
/// bytes written. Sends throttled `Progress` messages and honours `cancel`.
fn download_to(
    spec: &ToolSpec,
    dest: &Path,
    tx: &Sender<DownloadMsg>,
    cancel: &AtomicBool,
) -> Result<String, InstallError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        // No overall timeout: a 1.4 GB file on slow DSL legitimately takes
        // a long time. Stalls are caught by the per-read timeout instead.
        .timeout_read(Duration::from_secs(60))
        .build();

    let response = agent
        .get(spec.url)
        .call()
        .map_err(|e| format!("{}: request failed: {e}", spec.display_name))?;

    let total: Option<u64> = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .or(Some(spec.approx_size));

    let mut reader = response.into_reader();
    let file = fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut writer = std::io::BufWriter::new(file);

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    let mut done: u64 = 0;
    let mut last_sent = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = fs::remove_file(dest);
            return Err(InstallError::Cancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("{}: download interrupted: {e}", spec.display_name))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("write failed ({}): {e}", dest.display()))?;
        done += n as u64;

        if last_sent.elapsed() >= Duration::from_millis(100) {
            let _ = tx.send(DownloadMsg::Progress(spec.id, done, total));
            last_sent = Instant::now();
        }
    }
    writer
        .flush()
        .map_err(|e| format!("write failed ({}): {e}", dest.display()))?;
    let _ = tx.send(DownloadMsg::Progress(spec.id, done, total));

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("open failed: {e}"))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid zip: {e}"))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        // enclosed_name() rejects absolute paths and `..` traversal.
        let rel = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let out_path = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("mkdir {}: {e}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut out = fs::File::create(&out_path)
            .map_err(|e| format!("create {}: {e}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("extract {}: {e}", out_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// Unpack a .7z, reporting progress and honouring cancellation.
///
/// This is the slowest step by far — the whisper engine is 1.4 GB compressed
/// and ~4.5 GB expanded — so it must not look like a hang. It reports bytes
/// written against the expanded total, and it checks `cancel` between entries
/// so the Cancel button actually stops it (a plain `decompress_file` runs to
/// completion no matter what the user clicks).
fn extract_7z(
    archive: &Path,
    dest: &Path,
    id: ToolId,
    tx: &Sender<DownloadMsg>,
    cancel: &AtomicBool,
) -> Result<(), InstallError> {
    // Header-only pass to learn the expanded size, so the UI can show a real
    // bar rather than an indeterminate spinner. Cheap: this parses the archive
    // metadata and decompresses nothing. If it fails we carry on with a total
    // of 0, which the UI reads as "unknown".
    let total: u64 = sevenz_rust::SevenZReader::open(archive, sevenz_rust::Password::empty())
        .map(|reader| {
            reader
                .archive()
                .files
                .iter()
                .filter(|f| !f.is_directory())
                .map(|f| f.size())
                .sum()
        })
        .unwrap_or(0);

    let mut done: u64 = 0;
    let mut last_sent = Instant::now();
    let mut cancelled = false;

    let result =
        sevenz_rust::decompress_file_with_extract_fn(archive, dest, |entry, reader, path| {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                // Returning false stops the walk early — and does so as an Ok,
                // hence the separate flag to tell it apart from a clean finish.
                return Ok(false);
            }

            let keep_going = sevenz_rust::default_entry_extract_fn(entry, reader, path)?;

            if !entry.is_directory() {
                done += entry.size();
                if last_sent.elapsed() >= Duration::from_millis(150) {
                    let _ = tx.send(DownloadMsg::Extracting(id, done, total));
                    last_sent = Instant::now();
                }
            }
            Ok(keep_going)
        });

    if cancelled {
        return Err(InstallError::Cancelled);
    }
    result.map_err(|e| InstallError::Other(format!("7z extraction failed: {e}")))?;

    let _ = tx.send(DownloadMsg::Extracting(id, total, total));
    Ok(())
}

// ---------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------

/// "312 MB / 1.4 GB" style progress label.
pub fn progress_label(done: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => format!("{} / {}", human_bytes(done), human_bytes(t)),
        _ => human_bytes(done),
    }
}


/// The managed copy of a CLI launcher, if the download manager handles one.
/// Only yt-dlp qualifies: whisperx and demucs are Python modules with no
/// standalone exe, so they stay on the pipx/python detection path.
pub fn managed_cli(cli_name: &str) -> Option<PathBuf> {
    match cli_name {
        "yt-dlp" => installed_exe(ToolId::YtDlp),
        _ => None,
    }
}


/// PATH value with the bundled/managed dirs of ffmpeg, deno and yt-dlp
/// prepended, for child processes that discover helpers via PATH — yt-dlp
/// locates deno that way. `None` means there's nothing to add and the
/// child can inherit the environment untouched.
pub fn path_env_with_tools() -> Option<std::ffi::OsString> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(bin) = bundled_bin_dir() {
        if bin.is_dir() {
            dirs.push(bin);
        }
    }
    for id in [ToolId::Ffmpeg, ToolId::Deno, ToolId::YtDlp] {
        if let Some(exe) = installed_exe(id) {
            if let Some(parent) = exe.parent() {
                if !dirs.iter().any(|d| d == parent) {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
    }
    if dirs.is_empty() {
        return None;
    }
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(dirs).ok()
}

/// Directory containing a bundled/managed ffmpeg, for yt-dlp's
/// `--ffmpeg-location` flag. The bundled `bin\` dir is not on PATH, so a
/// yt-dlp we spawn cannot find the ffmpeg sitting next to it unless told.
/// `None` means ffmpeg resolves via PATH and no flag is needed.
pub fn ffmpeg_location_dir() -> Option<PathBuf> {
    let resolved = resolve_program("ffmpeg");
    if resolved.is_absolute() {
        resolved.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

pub fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.2} GB", n / GB)
    } else if n >= MB {
        format!("{:.0} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}
