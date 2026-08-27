//! Two dependency probes, one per tool the UI can prompt about: Whisper (the
//! required transcription engine) and Demucs (the optional music-stripping
//! model). Each has its own window in the UI, so there's no aggregate check
//! here — ffmpeg/deno/yt-dlp are shipped in the installer's `bin\` dir and
//! resolved at spawn time rather than surfaced to the user.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::pipeline::no_window_command;

/// Set once a whisperx run has actually failed. See [`mark_whisperx_unusable`].
static WHISPERX_UNUSABLE: AtomicBool = AtomicBool::new(false);

/// Record that whisperx launched but could not transcribe.
///
/// The probe below can only ask cheap questions — "is it there", "does
/// `--version` answer" — and a whisperx whose CUDA libraries are missing
/// passes both, then dies part-way into the first real run. Left at that, the
/// app would go on believing it has an engine: it would never offer the
/// standalone download, and every job would fail the same way.
///
/// Deliberately not persisted. If the user repairs their install, restarting
/// the app is enough to get whisperx reconsidered.
pub fn mark_whisperx_unusable() {
    WHISPERX_UNUSABLE.store(true, Ordering::Relaxed);
}

/// Whether a whisperx run has failed this session.
pub fn whisperx_unusable() -> bool {
    WHISPERX_UNUSABLE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug)]
pub enum DependencyStatus {
    /// Present: found on PATH, in the bundled dir, or as a Python module.
    Found,
    /// Not found anywhere we look.
    NotFound,
}

impl DependencyStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, DependencyStatus::Found)
    }
}

/// Install tips shown in each tool's window when it's missing. Kept as
/// constants so the UI can display them every frame without re-probing (the
/// `check_*` functions spawn subprocesses and must not run in the render loop).
pub const WHISPER_INSTALL_HINT: &str =
    "Recommended: use the Download button above — the standalone engine needs \
     no Python.\n\n\
     Or install whisperx yourself:\n\
     \u{20}   pip install --user whisperx\n\
     (see https://github.com/m-bain/whisperX for CUDA setup)";

pub const DEMUCS_INSTALL_HINT: &str =
    "macOS/Linux:  pipx install demucs\n\
     Windows:      pip install --user demucs\n\n\
     If pipx is missing, first run:\n\
     \u{20}   macOS:    brew install pipx\n\
     \u{20}   Windows:  python -m pip install --user pipx\n\n\
     The first run also downloads ~330 MB of model weights.";

/// Demucs: an optional Python model that separates a track into vocals and
/// instruments. "Strip music" runs it first so backing music doesn't confuse
/// the transcription.
///
/// Blocking, and slower than it looks: finding it means importing the module,
/// which pulls in torch and can take seconds on a cold cache. Call
/// [`refresh_demucs`] instead from anywhere the UI thread can reach.
pub fn check_demucs() -> DependencyStatus {
    check_python_module("demucs", "demucs")
}

// ---------------------------------------------------------------------------
// Demucs, off the UI thread
// ---------------------------------------------------------------------------

const DEMUCS_UNKNOWN: u8 = 0;
const DEMUCS_FOUND: u8 = 1;
const DEMUCS_MISSING: u8 = 2;

/// Last known answer from [`check_demucs`], so the render loop never has to ask.
static DEMUCS: AtomicU8 = AtomicU8::new(DEMUCS_UNKNOWN);
/// Whether a probe is in flight, so repeated clicks don't pile up threads.
static DEMUCS_PROBING: AtomicBool = AtomicBool::new(false);

/// What we currently believe about demucs, or `None` if we have not found out
/// yet.
///
/// Cheap enough to call every frame - one atomic load.
pub fn demucs_status() -> Option<DependencyStatus> {
    match DEMUCS.load(Ordering::Relaxed) {
        DEMUCS_FOUND => Some(DependencyStatus::Found),
        DEMUCS_MISSING => Some(DependencyStatus::NotFound),
        _ => None,
    }
}

/// Probe for demucs on a helper thread, updating [`demucs_status`] when it
/// lands.
///
/// Started once at launch so the answer is usually there before anyone asks,
/// and again whenever the user says they have installed it. Does nothing if a
/// probe is already running: the button is clickable repeatedly and the work is
/// heavy enough that stacking it up would be its own problem.
pub fn refresh_demucs() {
    if DEMUCS_PROBING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let status = check_demucs();
        DEMUCS.store(
            if status.is_ok() {
                DEMUCS_FOUND
            } else {
                DEMUCS_MISSING
            },
            Ordering::Relaxed,
        );
        DEMUCS_PROBING.store(false, Ordering::SeqCst);
    });
}

/// Transcription, the one hard requirement: it turns the audio into the
/// sentences everything downstream cuts and repeats. Satisfied by the
/// downloaded Faster-Whisper-XXL engine (preferred — no Python) or a system
/// whisperx. Probed at startup and before each run.
pub fn check_transcription() -> DependencyStatus {
    if crate::download::installed_exe(crate::download::ToolId::FasterWhisper).is_some() {
        return DependencyStatus::Found;
    }

    // A whisperx that has already failed a real run doesn't count as an engine,
    // whatever `--version` says — reporting it as present is what would hide
    // the download that fixes the problem.
    if whisperx_unusable() {
        return DependencyStatus::NotFound;
    }

    check_python_module("whisperx", "whisperx")
}

fn check_python_module(module: &'static str, cli_name: &'static str) -> DependencyStatus {
    // A copy from the in-app download manager first, then pipx launchers.
    let mut launchers = pipx_launcher_candidates(cli_name);
    if let Some(managed) = crate::download::managed_cli(cli_name) {
        launchers.insert(0, managed);
    }
    for launcher in launchers {
        if launcher.exists() {
            if let Ok(o) = no_window_command(&launcher).arg("--version").output() {
                if o.status.success() {
                    return DependencyStatus::Found;
                }
            }
        }
    }

    // Otherwise, probe candidate Pythons for the module.
    let probe = format!(
        "import importlib; importlib.import_module('{module}')",
        module = module
    );

    for candidate in python_candidates() {
        if let Ok(o) = no_window_command(&candidate).args(["-c", &probe]).output() {
            if o.status.success() {
                return DependencyStatus::Found;
            }
        }
    }

    DependencyStatus::NotFound
}

#[cfg(target_os = "windows")]
fn pipx_launcher_candidates(cli_name: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let exe = format!("{cli_name}.exe");
    if let Some(home) = std::env::var_os("USERPROFILE") {
        out.push(std::path::PathBuf::from(&home).join(".local").join("bin").join(&exe));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(
            std::path::PathBuf::from(&appdata)
                .join("Python")
                .join("Scripts")
                .join(&exe),
        );
    }
    // winget package directory.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let winget_packages = std::path::PathBuf::from(&local)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages");
        if let Ok(entries) = std::fs::read_dir(&winget_packages) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&format!("{cli_name}.{cli_name}_"))
                    || name.starts_with(&format!("{cli_name}_"))
                {
                    out.push(entry.path().join(&exe));
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "windows"))]
fn pipx_launcher_candidates(cli_name: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(std::path::PathBuf::from(home).join(".local/bin").join(cli_name));
    }
    out
}

#[cfg(target_os = "windows")]
fn python_candidates() -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    out.push("py".into());
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        for ver in ["Python313", "Python312", "Python311", "Python310"] {
            out.push(
                std::path::PathBuf::from(&local)
                    .join("Programs")
                    .join("Python")
                    .join(ver)
                    .join("python.exe"),
            );
        }
    }
    out.push("python".into());
    out.push("python3".into());
    out
}

#[cfg(not(target_os = "windows"))]
fn python_candidates() -> Vec<std::path::PathBuf> {
    vec![
        "/opt/homebrew/bin/python3".into(),
        "/opt/homebrew/bin/python3.12".into(),
        "/opt/homebrew/bin/python3.11".into(),
        "/opt/homebrew/bin/python3.10".into(),
        "/usr/local/bin/python3".into(),
        "/usr/local/bin/python3.12".into(),
        "/usr/local/bin/python3.11".into(),
        "/usr/bin/python3".into(),
        "python3".into(),
    ]
}
