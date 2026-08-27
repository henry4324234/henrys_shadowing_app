use std::error::Error;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const SAMPLE_RATE: u32 = 44_100;

pub type BoxErr = Box<dyn Error + Send + Sync>;

/// A writable directory for spawned tools to stand in.
///
/// A child inherits our working directory, and a GUI app launched the way
/// users launch one — Finder, the Dock, `open` — is given `/`. Any tool that
/// then creates something by a *relative* path is writing to the read-only
/// system volume. The transcription engines do exactly that: both resolve
/// their model-download cache relative to the cwd, so on a normal launch the
/// first fetch of any model died with
///
/// ```text
/// OSError: [Errno 30] Read-only file system: '//.cache'
/// ```
///
/// which surfaced as "transcription engine failed (exit code 1)". It only bit
/// when a model still had to be downloaded, so it looked intermittent — but
/// for a new install every model is uncached, which made it the first thing a
/// user hit.
///
/// Under `managed_root` rather than a temp dir on purpose: that cache is worth
/// keeping. Somewhere that gets cleaned between jobs would re-download the
/// model every run, which is 484 MB for `small`.
fn tool_working_dir() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = crate::download::managed_root()?.join("cache");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    })
    .as_deref()
}

/// Build a `Command` that won't pop up a console window on Windows, and that
/// runs somewhere it is allowed to write. On non-Windows the first part is
/// just `Command::new`; the working directory matters everywhere.
///
/// Used for every external tool (ffmpeg, yt-dlp, demucs, whisperx, python), so
/// a release GUI build neither flashes terminal windows nor inherits a working
/// directory it cannot write to. See [`tool_working_dir`].
pub fn no_window_command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000
        cmd.creation_flags(0x0800_0000);
    }
    // Every path we hand these tools is absolute, so this only changes where
    // relative paths they invent themselves land.
    if let Some(dir) = tool_working_dir() {
        cmd.current_dir(dir);
    }
    cmd
}

/// Force a Python child process to write its stdout/stderr as UTF-8.
///
/// Python does not use UTF-8 for a *pipe* on Windows — it uses the locale
/// encoding, CP1252 on most machines. Text then comes back as bytes that aren't
/// valid UTF-8, and `from_utf8_lossy` turns every non-ASCII character into a
/// U+FFFD box.
///
/// **This does not work for yt-dlp**, which overrides Python's stdio setup with
/// its own encoding handling — it needs the `--encoding utf-8` *flag* instead
/// (verified: with only this env var set, yt-dlp still emits CP1252). Use this
/// for the plain-Python tools, whisperx and demucs, whose stderr we surface as
/// error messages.
///
/// Not needed for tools that write their output to a *file* (whisper's SRT and
/// JSON), which are UTF-8 already.
fn utf8_output(cmd: &mut Command) -> &mut Command {
    cmd.env("PYTHONIOENCODING", "utf-8")
}

/// yt-dlp's own flag for the above. Every yt-dlp invocation whose stdout we
/// read must pass this, or non-ASCII text (video titles, and file paths on a
/// machine whose username has an accent) comes back mangled.
const YT_DLP_UTF8: [&str; 2] = ["--encoding", "utf-8"];

/// Returns a command (program + optional `-m module` args) that can invoke
/// the named module. Tries pipx-installed launchers first, then falls back
/// to `python -m module` across several candidate Pythons.
fn find_module_invocation(module: &str, cli_name: &str) -> Option<(PathBuf, Vec<String>)> {
    // A copy installed by the in-app download manager or bundled by the
    // installer wins over pipx/PATH copies — it's the version we pinned.
    let resolved = crate::download::resolve_program(cli_name);
    if resolved.is_absolute() {
        return Some((resolved, Vec::new()));
    }

    // pipx launcher locations vary by platform. On macOS/Linux it's
    // ~/.local/bin/<name>; on Windows it's %USERPROFILE%\.local\bin\<name>.exe
    // (and sometimes %APPDATA%\Python\Scripts\<name>.exe).
    for candidate in pipx_launcher_candidates(cli_name) {
        if candidate.exists() {
            let probe = no_window_command(&candidate).arg("--version").output();
            if matches!(probe, Ok(o) if o.status.success() || !o.stderr.is_empty()) {
                return Some((candidate, Vec::new()));
            }
        }
    }

    // Fall back to `python -m <module>` against candidate interpreters.
    for candidate in python_candidates() {
        let check = no_window_command(&candidate)
            .args(["-c", &format!("import {module}")])
            .output();
        if let Ok(o) = check {
            if o.status.success() {
                return Some((candidate, vec!["-m".to_string(), module.to_string()]));
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn pipx_launcher_candidates(cli_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let exe = format!("{cli_name}.exe");
    if let Some(home) = std::env::var_os("USERPROFILE") {
        out.push(PathBuf::from(&home).join(".local").join("bin").join(&exe));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        // Older pipx versions and pip --user both put scripts here.
        out.push(
            PathBuf::from(&appdata)
                .join("Python")
                .join("Scripts")
                .join(&exe),
        );
    }
    // winget package directory. The path includes a hash so we glob for
    // any subdirectory matching the package id pattern.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let winget_packages = PathBuf::from(&local)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages");
        if let Ok(entries) = std::fs::read_dir(&winget_packages) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // winget package dirs look like
                //   <publisher>.<package>_<hash>
                // e.g. "yt-dlp.yt-dlp_Microsoft.Winget.Source_8wekyb3d8bbwe"
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
fn pipx_launcher_candidates(cli_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".local/bin").join(cli_name));
    }
    out
}

#[cfg(target_os = "windows")]
fn python_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // py launcher first — the standard way to run Python on Windows.
    out.push("py".into());
    // Common per-user installs from python.org.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        for ver in ["Python313", "Python312", "Python311", "Python310"] {
            out.push(
                PathBuf::from(&local)
                    .join("Programs")
                    .join("Python")
                    .join(ver)
                    .join("python.exe"),
            );
        }
    }
    // Whatever PATH gives us.
    out.push("python".into());
    out.push("python3".into());
    out
}

#[cfg(not(target_os = "windows"))]
fn python_candidates() -> Vec<PathBuf> {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CookieSource {
    None,
    Chrome,
    Brave,
    Edge,
    Firefox,
    Safari,
    File(PathBuf),
}

impl CookieSource {
    pub fn label(&self) -> &'static str {
        match self {
            CookieSource::None => "None",
            CookieSource::Chrome => "Chrome",
            CookieSource::Brave => "Brave",
            CookieSource::Edge => "Edge",
            CookieSource::Firefox => "Firefox",
            CookieSource::Safari => "Safari",
            CookieSource::File(_) => "Cookies file",
        }
    }

    pub fn yt_dlp_value(&self) -> Option<&'static str> {
        match self {
            CookieSource::None | CookieSource::File(_) => None,
            CookieSource::Chrome => Some("chrome"),
            CookieSource::Brave => Some("brave"),
            CookieSource::Edge => Some("edge"),
            CookieSource::Firefox => Some("firefox"),
            CookieSource::Safari => Some("safari"),
        }
    }

    /// Path to a Netscape-format cookies.txt, if this variant is File.
    pub fn cookies_file(&self) -> Option<&Path> {
        match self {
            CookieSource::File(p) => Some(p.as_path()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Separating,
    Decoding,
    Transcribing,
    Translating,
    Splitting,
    Encoding,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Downloading => "Downloading From Youtube",
            Stage::Separating => "Removing Music",
            Stage::Decoding => "Decoding Audio",
            Stage::Transcribing => "Transcribing To Text",
            Stage::Translating => "Translating Text Into English",
            Stage::Splitting => "Creating Audio Chunks",
            Stage::Encoding => "Generating Output File",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3,
}

impl WhisperModel {
    /// Every accuracy the picker offers, smallest first. One list, so a new
    /// model cannot be added to the dropdown and quietly forgotten by the code
    /// that asks what is installed.
    pub const ALL: [WhisperModel; 5] = [
        WhisperModel::Tiny,
        WhisperModel::Base,
        WhisperModel::Small,
        WhisperModel::Medium,
        WhisperModel::LargeV3,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            WhisperModel::Tiny => "Lowest Accuracy, Fastest",
            WhisperModel::Base => "Low Accuracy, Fast",
            WhisperModel::Small => "Medium Accuracy",
            WhisperModel::Medium => "High Accuracy, Slow",
            WhisperModel::LargeV3 => "Highest Accuracy, Slowest",
        }
    }

    pub fn whisperx_value(&self) -> &'static str {
        match self {
            WhisperModel::Tiny => "tiny",
            WhisperModel::Base => "base",
            WhisperModel::Small => "small",
            WhisperModel::Medium => "medium",
            WhisperModel::LargeV3 => "large-v3",
        }
    }

    /// The GGML file this model is distributed as, without a path.
    pub fn ggml_file(&self) -> &'static str {
        match self {
            WhisperModel::Tiny => "ggml-tiny.bin",
            WhisperModel::Base => "ggml-base.bin",
            WhisperModel::Small => "ggml-small.bin",
            WhisperModel::Medium => "ggml-medium.bin",
            WhisperModel::LargeV3 => "ggml-large-v3.bin",
        }
    }

    /// What `--dtw` wants. Its own spelling, close to but not the same as the
    /// filename: dots rather than hyphens for the large variants, and it
    /// rejects anything off its list rather than ignoring it.
    fn whispercpp_dtw_value(&self) -> &'static str {
        match self {
            WhisperModel::Tiny => "tiny",
            WhisperModel::Base => "base",
            WhisperModel::Small => "small",
            WhisperModel::Medium => "medium",
            WhisperModel::LargeV3 => "large.v3",
        }
    }
}

pub fn run_pipeline(
    source: &Source,
    output_dir: &Path,
    whisper_model: WhisperModel,
    strip_music: bool,
    cookie_source: CookieSource,
    repeat_count: usize,
    gap_ratio: f64,
    max_chunk_seconds: f64,
    max_duration_seconds: Option<f64>,
    show_text: bool,
    translate_english: bool,
    // Set from another thread when this job should stop. Checked between stages
    // and inside the long streaming stages (download, transcription, encoding),
    // which kill their child process and return promptly when it flips. The
    // caller treats any early return with this set as a cancellation, not a
    // failure.
    cancel: &AtomicBool,
    on_progress: &dyn Fn(Stage, Option<f32>),
) -> Result<PathBuf, BoxErr> {
    let mut cleanup_files: Vec<PathBuf> = Vec::new();
    let mut cleanup_dirs: Vec<PathBuf> = Vec::new();

    // Bail out at a stage boundary if the job has been cancelled. The streaming
    // stages have their own inner checks; this catches the gaps between them and
    // the blocking stages (music separation, decoding) that can't be interrupted
    // mid-run.
    let cancelled = || -> BoxErr { "job cancelled".into() };

    let result = (|| -> Result<PathBuf, BoxErr> {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        // Resolve the input audio file.
        let mut audio_path = match source {
            Source::File(p) => p.clone(),
            Source::Url(url) => {
                on_progress(Stage::Downloading, None);
                let p = download_audio_from_url(url, cookie_source, cancel, &|frac| {
                    on_progress(Stage::Downloading, Some(frac));
                })?;
                // Clean up both the file and its (unique) parent dir.
                if let Some(parent) = p.parent() {
                    cleanup_dirs.push(parent.to_path_buf());
                } else {
                    cleanup_files.push(p.clone());
                }
                p
            }
        };

        // Capture the naming stem BEFORE potential separation, so the
        // output is named after the original source rather than "vocals".
        let stem = audio_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());

        // Optionally isolate vocals via Demucs.
        if strip_music {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            on_progress(Stage::Separating, None);
            let work_dir = unique_work_dir("demucs");
            let vocals = separate_vocals(&audio_path, &work_dir)?;
            cleanup_dirs.push(work_dir);
            audio_path = vocals;
        }

        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        on_progress(Stage::Transcribing, None);
        let mut segments =
            transcribe_to_segments(&audio_path, whisper_model, Task::Transcribe, cancel, &|frac| {
                on_progress(Stage::Transcribing, Some(frac))
            })?;

        // Optional second pass: an English translation of the same audio,
        // aligned back onto the original sentences so each can be shown beneath
        // its source text. Best-effort — a translation failure leaves the
        // original transcript intact rather than sinking the whole job.
        if translate_english {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            on_progress(Stage::Translating, None);
            if let Ok(english) =
                transcribe_to_segments(&audio_path, whisper_model, Task::Translate, cancel, &|frac| {
                    on_progress(Stage::Translating, Some(frac))
                })
            {
                attach_translations(&mut segments, &english);
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        on_progress(Stage::Decoding, None);
        let mut samples = decode_audio_to_samples(&audio_path)?;

        // Trim to max duration if requested.
        //
        // The transcript is *not* trimmed by this: transcription ran against
        // the whole file, above, so it still describes audio that no longer
        // exists. Left alone, a segment straddling the cut keeps word times
        // past the end of the buffer, the splitter picks a cut from them, and
        // the slice that follows starts after it ends - which panics the job
        // thread and leaves the UI sitting on a percentage forever.
        //
        // So bring the segments down to the audio we kept: drop the ones that
        // start past the end, and clamp the one that straddles it, words and
        // all.
        if let Some(max_secs) = max_duration_seconds {
            let max_samples = (max_secs * SAMPLE_RATE as f64) as usize;
            if samples.len() > max_samples {
                samples.truncate(max_samples);
            }
            let limit = samples.len() as f64 / SAMPLE_RATE as f64;
            clamp_segments_to(&mut segments, limit);
        }

        let output_path = output_dir.join(format!("{stem}.mp4"));

        let log_path = output_dir.join(format!("{stem}.split-log.txt"));

        let log_file = std::fs::File::create(&log_path)?;

        let mut split_log = BufWriter::new(log_file);

        writeln!(split_log, "AUDIO SPLITTING LOG")?;
        writeln!(split_log, "===================")?;
        writeln!(split_log, "Output: {}", output_path.display())?;
        writeln!(split_log, "Sample rate: {SAMPLE_RATE}")?;
        writeln!(
            split_log,
            "Maximum chunk length: {max_chunk_seconds:.3} seconds"
        )?;
        writeln!(split_log, "Whisper segments: {}", segments.len())?;
        writeln!(split_log)?;

        split_log.flush()?;

        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        on_progress(Stage::Splitting, Some(0.0));
        let chunks = segments_to_chunks(
            &samples,
            &segments,
            max_chunk_seconds,
            &mut split_log,
            &|frac| on_progress(Stage::Splitting, Some(frac)),
        )?;
        split_log.flush()?;

        let (repeated, subtitles) = repeat_chunks(&chunks, repeat_count, gap_ratio);

        // Generate a subtitle file if the user wants on-screen text — either the
        // original transcript, or the original plus its English translation.
        let subtitle_path: Option<PathBuf> =
            if (show_text || translate_english) && !subtitles.is_empty() {
                let path = unique_work_dir("subs").with_extension("ass");
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match write_ass_subtitles(&path, &subtitles) {
                    Ok(()) => {
                        cleanup_files.push(path.clone());
                        Some(path)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        on_progress(Stage::Encoding, Some(0.0));
        let total_seconds = repeated.len() as f64 / SAMPLE_RATE as f64;
        encode_samples_to_mp4(
            &repeated,
            &output_path,
            subtitle_path.as_deref(),
            total_seconds,
            cancel,
            &|frac| on_progress(Stage::Encoding, Some(frac)),
        )?;

        Ok(output_path)
    })();

    for p in cleanup_files {
        let _ = std::fs::remove_file(p);
    }
    for d in cleanup_dirs {
        let _ = std::fs::remove_dir_all(d);
    }

    result
}

static WORK_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ask yt-dlp for a URL's title without downloading anything, so the queue can
/// show "Chopin — Nocturne Op.9 No.2" instead of a wall of query string.
///
/// Best-effort by design: returns None if yt-dlp is missing, the site refuses,
/// or the fetch fails for any reason. Callers fall back to showing the raw URL,
/// so a failure here costs nothing. Runs a subprocess — call it off the UI
/// thread.
pub fn fetch_video_title(url: &str, cookie_source: CookieSource) -> Option<String> {
    let mut args: Vec<String> = vec![
        // The title is the whole point here, and titles are full of accents and
        // non-Latin scripts. Without this they arrive as CP1252 and every such
        // character shows up as a box.
        YT_DLP_UTF8[0].into(),
        YT_DLP_UTF8[1].into(),
        "--no-playlist".into(),
        // Same JS-challenge opt-in as the audio download — without it YouTube
        // can refuse to hand over even the metadata.
        "--remote-components".into(),
        "ejs:github".into(),
        "--quiet".into(),
        "--no-warnings".into(),
        // --print implies --simulate, so nothing is downloaded.
        "--print".into(),
        "%(title)s".into(),
    ];

    if let Some(browser) = cookie_source.yt_dlp_value() {
        args.push("--cookies-from-browser".into());
        args.push(browser.into());
    } else if let Some(path) = cookie_source.cookies_file() {
        args.push("--cookies".into());
        args.push(path.to_string_lossy().into_owned());
    }

    args.push(url.into());

    let mut cmd = match find_module_invocation("yt_dlp", "yt-dlp") {
        Some((program, prefix_args)) => {
            let mut c = no_window_command(&program);
            c.args(&prefix_args);
            c
        }
        None => no_window_command("yt-dlp"),
    };
    // yt-dlp finds deno via PATH; a bundled/downloaded deno lives outside it.
    if let Some(path) = crate::download::path_env_with_tools() {
        cmd.env("PATH", path);
    }

    let output = cmd.args(&args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let title = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();

    (!title.is_empty()).then_some(title)
}

fn unique_work_dir(prefix: &str) -> PathBuf {
    let n = WORK_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("speech-repeater")
        .join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// Parse one yt-dlp output line into a download fraction (0..=1).
///
/// Matches the machine-readable line our `--progress-template` asks for
/// ("PROGRESS  42.3%"), plus yt-dlp's default human format
/// ("[download]  42.3% of ...") as a belt-and-braces fallback. Lines with an
/// unknown total print "N/A" instead of a number and simply don't parse.
fn parse_ytdlp_progress(line: &str) -> Option<f32> {
    let rest = line
        .trim_start()
        .strip_prefix("PROGRESS")
        .or_else(|| line.trim_start().strip_prefix("[download]"))?;
    let token = rest.split_whitespace().find(|t| t.ends_with('%'))?;
    let pct: f32 = token.trim_end_matches('%').parse().ok()?;
    Some((pct / 100.0).clamp(0.0, 1.0))
}

fn download_audio_from_url(
    url: &str,
    cookie_source: CookieSource,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f32),
) -> Result<PathBuf, BoxErr> {
    // Each download gets its own subdirectory so parallel jobs don't
    // collide on .part / fragment files.
    let temp_dir = unique_work_dir("ytdlp");
    std::fs::create_dir_all(&temp_dir)?;

    let output_template = temp_dir.join("%(title)s.%(ext)s");

    // Build the argument list once, then dispatch to either the pipx
    // launcher or `python -m yt_dlp`.
    let mut args: Vec<String> = vec![
        // We parse a real file path out of this command's stdout. Left at the
        // locale encoding, any non-ASCII in that path (an accented Windows
        // username, say) comes back mangled and the downloaded file can't be
        // found afterwards.
        YT_DLP_UTF8[0].into(),
        YT_DLP_UTF8[1].into(),
        // Format selector with fallbacks: prefer m4a-only audio, then any
        // audio-only stream, then the audio half of any combined stream,
        // then anything at all. yt-dlp resolves them in order and uses
        // the first one YouTube actually serves.
        "-f".into(),
        "bestaudio[ext=m4a]/bestaudio/bestaudio*/best".into(),
        // YouTube now requires solving a JS "n challenge" to get real
        // media URLs. yt-dlp delegates to Deno but won't auto-download
        // the solver script without explicit opt-in. The script is
        // cached after first use.
        "--remote-components".into(),
        "ejs:github".into(),
        "--no-playlist".into(),
        "--restrict-filenames".into(),
        "--quiet".into(),
        "--no-warnings".into(),
        // Real progress for the GUI bar even in quiet mode: one easily
        // parsed line per update ("PROGRESS  42.3%"), each on its own
        // line rather than \r-rewritten. In quiet mode yt-dlp writes
        // these to stderr, away from the filepath we print on stdout —
        // but the reader below accepts them from either stream.
        "--progress".into(),
        "--newline".into(),
        "--progress-template".into(),
        "download:PROGRESS %(progress._percent_str)s".into(),
        "--print".into(),
        "after_move:filepath".into(),
        "-o".into(),
        output_template.to_string_lossy().into_owned(),
    ];

    // A bundled/downloaded ffmpeg isn't on PATH, so yt-dlp must be told
    // where it is for its post-processing steps.
    if let Some(dir) = crate::download::ffmpeg_location_dir() {
        args.push("--ffmpeg-location".into());
        args.push(dir.to_string_lossy().into_owned());
    }

    if let Some(browser) = cookie_source.yt_dlp_value() {
        args.push("--cookies-from-browser".into());
        args.push(browser.into());
    } else if let Some(path) = cookie_source.cookies_file() {
        args.push("--cookies".into());
        args.push(path.to_string_lossy().into_owned());
    }

    args.push(url.into());

    let mut cmd = if let Some((program, prefix_args)) = find_module_invocation("yt_dlp", "yt-dlp") {
        let mut c = no_window_command(&program);
        c.args(&prefix_args);
        c
    } else {
        no_window_command("yt-dlp")
    };
    // yt-dlp finds deno by searching PATH; a bundled/downloaded deno lives
    // outside PATH, so prepend those dirs for this child only.
    if let Some(path) = crate::download::path_env_with_tools() {
        cmd.env("PATH", path);
    }
    let spawned = cmd
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("yt-dlp not found. Install with: pipx install yt-dlp".into());
        }
        Err(e) => return Err(format!("failed to launch yt-dlp: {e}").into()),
    };

    // Forward both streams, line by line, into one channel. Two reader
    // threads mean neither pipe can fill up and stall yt-dlp while we're
    // blocked on the other one — and it doesn't matter which stream the
    // progress lines arrive on (stderr, in quiet mode). The progress
    // callback can't leave this thread (it's not Send), so all parsing
    // happens here on the receiving end.
    enum StreamLine {
        Out(String),
        Err(String),
    }
    let (line_tx, line_rx) = std::sync::mpsc::channel::<StreamLine>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = line_tx.clone();
        readers.push(std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(StreamLine::Out(line)).is_err() {
                    break;
                }
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = line_tx.clone();
        readers.push(std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                if tx.send(StreamLine::Err(line)).is_err() {
                    break;
                }
            }
        }));
    }
    // Drop our own sender so the loop below ends when both readers finish.
    drop(line_tx);

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut best = 0.0_f32;
    // recv_timeout rather than a plain recv so a cancellation is noticed within
    // ~150ms even when yt-dlp is stalled and emitting nothing. Killing the child
    // closes both pipes, which ends the reader threads we join below.
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = child.kill();
            break;
        }
        let line = match line_rx.recv_timeout(std::time::Duration::from_millis(150)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let (text, from_stdout) = match &line {
            StreamLine::Out(s) => (s.as_str(), true),
            StreamLine::Err(s) => (s.as_str(), false),
        };
        if let Some(frac) = parse_ytdlp_progress(text) {
            // Monotonic: a fallback format that downloads several files
            // restarts its percentage, and a bar that jumps backwards
            // reads as broken.
            if frac > best {
                best = frac;
                on_progress(frac);
            }
        } else if from_stdout {
            stdout_text.push_str(text);
            stdout_text.push('\n');
        } else {
            stderr_text.push_str(text);
            stderr_text.push('\n');
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;

    if cancelled {
        return Err("download cancelled".into());
    }

    if !status.success() {
        let exit_code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let tail = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let detail = if !stderr_text.trim().is_empty() {
            tail(&stderr_text)
        } else if !stdout_text.trim().is_empty() {
            tail(&stdout_text)
        } else {
            "no error output".to_string()
        };
        return Err(format!("yt-dlp exited with code {exit_code}: {detail}").into());
    }

    // The filepath from --print is the last non-progress line on stdout.
    let path_str = stdout_text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if path_str.is_empty() {
        return Err("yt-dlp returned no filepath".into());
    }

    Ok(PathBuf::from(path_str))
}

#[cfg(test)]
mod ytdlp_progress_tests {
    use super::parse_ytdlp_progress;

    #[test]
    fn parses_template_and_default_formats() {
        assert_eq!(
            parse_ytdlp_progress("PROGRESS  42.3%"),
            Some(42.3_f32 / 100.0)
        );
        assert_eq!(parse_ytdlp_progress("PROGRESS 100.0%"), Some(1.0));
        assert_eq!(
            parse_ytdlp_progress("[download]   7.5% of 10.00MiB at 2.00MiB/s"),
            Some(7.5_f32 / 100.0)
        );
    }

    #[test]
    fn ignores_everything_else() {
        assert_eq!(parse_ytdlp_progress("PROGRESS    N/A"), None);
        assert_eq!(parse_ytdlp_progress("[download] Destination: x.m4a"), None);
        assert_eq!(parse_ytdlp_progress("C:\\temp\\clip.m4a"), None);
        assert_eq!(parse_ytdlp_progress(""), None);
    }
}

/// Ask PyTorch what accelerator it can use. Returns "cuda" for NVIDIA,
/// "mps" for Apple Silicon Metal, "cpu" otherwise. Cached per process —
/// the answer is static for the lifetime of the app.
fn pick_torch_device() -> String {
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            // We need an actual Python interpreter to run an import. The
            // demucs launcher isn't one, so search for a Python that has
            // torch installed (the same Python Demucs is using under the
            // hood).
            let Some((program, prefix_args)) = find_module_invocation("torch", "python3") else {
                return "cpu".to_string();
            };

            // find_module_invocation returns either:
            //  (a) a launcher with empty prefix_args (no use to us here), or
            //  (b) a python with prefix_args = ["-m", "torch"]
            // We want case (b) and to run `python -c "..."` instead of -m torch.
            if prefix_args.is_empty() {
                return "cpu".to_string();
            }

            let probe = "import torch; \
print('cuda' if torch.cuda.is_available() \
else 'mps' if getattr(torch.backends, 'mps', None) and torch.backends.mps.is_available() \
else 'cpu')";

            match no_window_command(&program).args(["-c", probe]).output() {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s == "cuda" || s == "mps" || s == "cpu" {
                        s
                    } else {
                        "cpu".to_string()
                    }
                }
                _ => "cpu".to_string(),
            }
        })
        .clone()
}

fn separate_vocals(input: &Path, work_dir: &Path) -> Result<PathBuf, BoxErr> {
    std::fs::create_dir_all(work_dir)?;

    let (program, prefix_args) =
        find_module_invocation("demucs", "demucs").ok_or_else(|| -> BoxErr {
            "demucs not installed. Install with: pipx install demucs".into()
        })?;

    // Pick the fastest accelerator the user's PyTorch supports.
    let device = pick_torch_device();

    // Disable tqdm's progress bars so a failure's real error isn't
    // drowned in download/processing progress text on stderr.
    // utf8_output so a failure's message (which may name an accented path)
    // survives into the UI intact rather than as replacement boxes.
    let result = utf8_output(&mut no_window_command(&program))
        .env("TQDM_DISABLE", "1")
        .args(&prefix_args)
        .args(["--two-stems=vocals", "-d", &device, "-j", "2", "-o"])
        .arg(work_dir)
        .arg(input)
        .output();

    let output = match result {
        Ok(o) => o,
        Err(e) => return Err(format!("failed to launch {}: {e}", program.display()).into()),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let exit_code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());

        // demucs is Python too, so its failures end in a traceback: lead
        // with the exception rather than the carets underneath it.
        let detail_source = if !stderr.trim().is_empty() {
            tool_error_detail(&stderr, 4)
        } else if !stdout.trim().is_empty() {
            tool_error_detail(&stdout, 4)
        } else {
            String::new()
        };
        let detail = if detail_source.is_empty() {
            "no error output".to_string()
        } else {
            detail_source
        };

        if detail.contains("No module named") && detail.contains("demucs") {
            return Err(
                "demucs is not installed for python3. Install with: pip3 install demucs".into(),
            );
        }
        return Err(format!("demucs exited with code {exit_code}: {detail}").into());
    }

    find_file_named(work_dir, "vocals.wav").ok_or_else(|| {
        format!(
            "demucs ran but produced no vocals.wav under {}",
            work_dir.display()
        )
        .into()
    })
}

fn find_file_named(dir: &Path, target: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, target) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == target) {
            return Some(path);
        }
    }
    None
}

fn decode_audio_to_samples(input: &Path) -> Result<Vec<i16>, BoxErr> {
    let sample_rate_text = SAMPLE_RATE.to_string();

    let result = no_window_command(crate::download::resolve_program("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args([
            "-ac",
            "1",
            "-ar",
            &sample_rate_text,
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "pipe:1",
        ])
        .output();

    let output = match result {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("ffmpeg not found in PATH. See https://ffmpeg.org".into());
        }
        Err(e) => return Err(e.into()),
    };

    if !output.status.success() {
        return Err(format!(
            "FFmpeg failed while decoding:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let raw = output.stdout;
    let sample_count = raw.len() / 2;
    let mut samples = Vec::with_capacity(sample_count);

    for bytes in raw.chunks_exact(2) {
        samples.push(i16::from_le_bytes([bytes[0], bytes[1]]));
    }

    Ok(samples)
}

/// One word, with the timing Whisper gave it. The text keeps its punctuation —
/// that's what tells us a gap follows a comma rather than a bare word boundary.
#[derive(Debug, Clone)]
struct Word {
    start: f64,
    end: f64,
    text: String,
}

/// One sentence-level time-span emitted by Whisper.
///
/// `words` may be empty: not every backend gives word timings, and when it's
/// empty the splitter falls back to cutting on silence instead of on word gaps.
#[derive(Debug, Clone)]
struct Segment {
    start: f64,
    end: f64,
    text: String,
    words: Vec<Word>,
    /// An English translation of `text`, when the user asked for one. Filled in
    /// by a second Whisper pass (see `attach_translations`); None otherwise.
    translation: Option<String>,
}

/// What Whisper should do with the audio: write it back in its own language, or
/// translate it to English. `translate` is a standard Whisper task and always
/// targets English — it's the whole reason the English-subtitle option can reuse
/// the engine that's already required, with no extra model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Task {
    Transcribe,
    Translate,
}

impl Task {
    fn whisper_value(&self) -> &'static str {
        match self {
            Task::Transcribe => "transcribe",
            Task::Translate => "translate",
        }
    }
}

/// Transcribe `input` into sentence-level segments (or translate it to English
/// when `task` is `Translate`).
///
/// Prefers the downloaded/bundled **Faster-Whisper-XXL** standalone engine
/// (no Python needed); falls back to a system **whisperx** install if the
/// engine isn't present. Both produce the same `Segment` shape, so the rest
/// of the pipeline doesn't care which ran.
fn transcribe_to_segments(
    input: &Path,
    model: WhisperModel,
    task: Task,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f32),
) -> Result<Vec<Segment>, BoxErr> {
    // whisper.cpp first where we have both halves of it. It is the only backend
    // that reaches the GPU on this machine, and it ships with the app rather
    // than being downloaded, so when it is present it is both the fastest
    // option and the one already on disk.
    //
    // Falls through rather than failing if the chosen model has not been
    // downloaded yet: the older engine can still do the job, slowly, which
    // beats refusing to start.
    let whispercpp = crate::download::resolve_program("whisper-cli");
    if whispercpp.is_file() {
        if let Some(model_file) = whispercpp_model_path(model) {
            return transcribe_with_whispercpp(
                &whispercpp,
                &model_file,
                input,
                model,
                task,
                cancel,
                on_progress,
            );
        }
    }

    if let Some(engine) = crate::download::installed_exe(crate::download::ToolId::FasterWhisper) {
        return transcribe_with_standalone(&engine, input, model, task, cancel, on_progress);
    }
    // Skip a whisperx that has already failed once this session. Falling
    // through to the "no engine available" message below points at the
    // download that fixes it, instead of failing the same way twice.
    if !crate::deps::whisperx_unusable()
        && find_module_invocation("whisperx", "whisperx").is_some()
    {
        return transcribe_with_whisperx(input, model, task, cancel, on_progress);
    }
    Err(
        "No transcription engine available. Open the dependency window and \
         download the transcription engine (recommended, no Python needed), \
         or install whisperx (pip install --user whisperx)."
            .into(),
    )
}

/// Faster-Whisper-XXL backend. Emits SRT (not JSON) on purpose: the engine's
/// `--sentence` sentence-splitting applies to every output format *except*
/// JSON, and sentence-level segments are exactly what this app is built on.
/// Whether the standalone engine at `engine` understands `flag`, by reading its
/// own `--help`. Probed once per process and cached: `--help` costs a subprocess
/// spawn, and the answer cannot change while the app is running.
///
/// This exists because two different engines answer to the same call site:
/// Windows gets Faster-Whisper-XXL, macOS the older plain Whisper-Faster (the
/// only Mac build Purfview ships), and an engine that does not know a flag dies
/// on it rather than ignoring it.
///
/// Worth knowing before trusting the comment over the code: r186.1, the pinned
/// Mac engine, documents both `--sentence` and `--beep_off` in its own help -
/// verified by running it. So this probe answers yes on both platforms today
/// and changes nothing. It earns its place as insurance for when the pin moves,
/// not as a workaround for a difference that currently exists.
fn engine_accepts(engine: &Path, flag: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static HELP: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);

    let mut guard = match HELP.lock() {
        Ok(g) => g,
        // A poisoned lock means a previous probe panicked; assume nothing.
        Err(_) => return false,
    };
    let cache = guard.get_or_insert_with(HashMap::new);
    let help = cache.entry(engine.to_path_buf()).or_insert_with(|| {
        no_window_command(engine)
            .arg("--help")
            .output()
            .map(|o| {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                text
            })
            .unwrap_or_default()
    });

    // An engine that printed no help at all is a probe failure, not a refusal.
    // Treat it as supporting the flag: that is what every build did before this
    // check existed, and losing `--sentence` silently would quietly coarsen
    // every transcription on Windows.
    help.is_empty() || help.contains(flag)
}

fn transcribe_with_standalone(
    engine: &Path,
    input: &Path,
    model: WhisperModel,
    task: Task,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f32),
) -> Result<Vec<Segment>, BoxErr> {
    let work_dir = unique_work_dir("fwxxl");
    std::fs::create_dir_all(&work_dir)?;

    // `--device auto` lets CTranslate2 pick CUDA when a usable GPU is present
    // and fall back to CPU otherwise. We deliberately do NOT reuse
    // pick_torch_device() here: it probes a Python+torch install, which the
    // standalone exists precisely to avoid — a GPU user with no Python would
    // be wrongly pinned to CPU. compute_type is left at the engine default
    // ("default" → float16 on CUDA, int8 on CPU).
    let mut args: Vec<String> = vec![input.to_string_lossy().into_owned()];
    for s in [
        "--model",
        model.whisperx_value(),
        // transcribe (source language) or translate (to English).
        "--task",
        task.whisper_value(),
        "--device",
        "auto",
        // `all` writes every format in one pass, which is how we get both at
        // once: the SRT carries the `--sentence` splitting (which does NOT
        // apply to JSON), and the JSON carries the word timings (which the SRT
        // has no way to express). One transcription, two things we need.
        "--output_format",
        "all",
        "--word_timestamps",
        "true",
        "--output_dir",
    ] {
        args.push(s.to_string());
    }
    args.push(work_dir.to_string_lossy().into_owned());
    // Sentence-level cues (VAD stays at the engine's silero default), and no
    // finish-beep. Both are Faster-Whisper-XXL extras: the plain Whisper-Faster
    // build - which is all Purfview publishes for macOS - rejects an unknown
    // flag outright and transcribes nothing, so ask it what it takes first.
    // Without `--sentence` the SRT carries the engine's own segmentation, which
    // the chunker then splits on pauses as usual; the result is coarser, not
    // broken.
    for flag in ["--sentence", "--beep_off"] {
        if engine_accepts(engine, flag) {
            args.push(flag.to_string());
        }
    }

    // Total duration up front (a cheap ffmpeg header read), so the engine's
    // per-segment "[.. --> MM:SS.mmm]" output can be turned into a real fraction
    // as it streams. 0 if it can't be determined — the bar then falls back to
    // the time-based estimate.
    let total_seconds = probe_duration_seconds(input).unwrap_or(0.0);

    // Stream the engine's output so its live progress can drive the bar,
    // instead of blocking on `.output()` and only learning it finished when it
    // finished. `status`/`stderr_text` stand in for what `.output()` gave us.
    let mut cmd = no_window_command(engine);
    cmd.args(&args);
    let (status, stderr_text) = run_engine_streaming(cmd, total_seconds, cancel, on_progress)
        .map_err(|e| -> BoxErr { format!("failed to launch transcription engine: {e}").into() })?;

    // If the job was cancelled mid-transcription the engine was killed and the
    // SRT is absent or partial; surface that as a cancellation rather than
    // letting the "no sentences" path below invent a scarier error.
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err("transcription cancelled".into());
    }

    // The engine writes <input_stem>.<ext> into the output dir.
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    let srt_path = work_dir.join(format!("{stem}.srt"));

    // Judge success by whether we got usable sentences, NOT by the exit code.
    // This engine can write a complete SRT and *then* crash on GPU teardown
    // (STATUS_STACK_BUFFER_OVERRUN / exit 0xC0000409) — a known CTranslate2
    // issue. Discarding a good transcription because the process died on its
    // way out would turn a working run into a failure, so we read the output
    // first and only consult the exit status if there's nothing usable.
    let mut segments = std::fs::read_to_string(&srt_path)
        .map(|t| parse_srt_segments(&t))
        .unwrap_or_default();

    if segments.is_empty() {
        // Nothing usable — now the exit status and stderr actually matter.
        // Strip the tqdm progress bars first, or the real error is buried under
        // a wall of "model.bin: 99%|####…" download-progress lines.
        let detail = meaningful_stderr_tail(&stderr_text, 6);
        let _ = std::fs::remove_dir_all(&work_dir);

        if !status.success() {
            let code = status
                .code()
                .map(|c| format!("exit code {c}"))
                .unwrap_or_else(|| "was terminated".to_string());
            let hint = if detail.is_empty() {
                // A native crash with no error text — almost always the GPU
                // library layer. First run downloads a large model, and the
                // very first GPU load after that can crash; a retry usually
                // succeeds because the model is then cached.
                "The engine crashed with no error message — this is usually a \
                 GPU driver/library issue. Try again (the model is downloaded \
                 now, so it won't repeat that step), or pick a smaller model."
                    .to_string()
            } else {
                detail
            };
            return Err(format!("transcription engine failed ({code}). {hint}").into());
        }
        return Err(format!(
            "transcription produced no sentences.{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {detail}")
            }
        )
        .into());
    }

    // Word timings come from the JSON side of the same run. Strictly an
    // enhancement: if the engine wrote no JSON, or wrote it in a shape we don't
    // recognise, the segments keep empty `words` and the splitter falls back to
    // cutting on silence. Never fail the transcription over it.
    let json_path = work_dir.join(format!("{stem}.json"));
    if let Ok(json_text) = std::fs::read_to_string(&json_path) {
        let words = parse_json_words(&json_text);
        if !words.is_empty() {
            attach_words(&mut segments, &words);
        }
    }

    let _ = std::fs::remove_dir_all(&work_dir);
    Ok(segments)
}

/// Run the standalone engine, streaming its output so live progress can be
/// reported, and returning `(exit status, combined output text)` once it exits —
/// the same two things `.output()` gave us, minus the wait-until-done.
///
/// The engine prints no percentage; it streams each transcribed segment as a
/// `[MM:SS.mmm --> MM:SS.mmm]  text` line on **stdout** as it goes. Because it
/// works through the audio in order, the end-timestamp of the latest segment
/// over the total duration is a true, monotonic progress fraction — better than
/// a percent bar. We parse that from stdout on this thread while a helper thread
/// drains stderr in parallel; draining both is what stops a full pipe on the
/// stream we're *not* reading from deadlocking the one we are. `total_seconds`
/// of 0 means "unknown" and disables live progress (the caller falls back to the
/// time estimate). Everything is kept verbatim for the error path, and dumped
/// for inspection when SHADOWING_DEBUG_STDERR is set.
fn run_engine_streaming(
    mut cmd: Command,
    total_seconds: f64,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f32),
) -> std::io::Result<(std::process::ExitStatus, String)> {
    use std::io::Read;
    use std::sync::mpsc::{self, RecvTimeoutError};

    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    // Drain stderr on a helper thread so it can't fill its pipe and deadlock us
    // while we're busy reading stdout. It owns the handle (Send + 'static), so
    // nothing borrowed crosses the thread boundary.
    let stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });

    // Read stdout on a *second* helper thread, forwarding raw chunks over a
    // channel. This thread then blocks on a timed recv instead of on the pipe,
    // so it can notice a cancellation and kill the engine even during the
    // initial model load, when it's producing no output to unblock a plain read.
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
    let stdout_handle = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if chunk_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    });

    let mut stdout_full: Vec<u8> = Vec::new();
    let mut last_frac: f32 = 0.0;
    let mut line: Vec<u8> = Vec::new();

    let mut report = |line: &[u8]| {
        if total_seconds <= 0.0 {
            return;
        }
        let text = String::from_utf8_lossy(line);
        if let Some(end) = parse_segment_end_seconds(&text) {
            // Cap just below full: segments finishing doesn't mean the SRT
            // is written yet, and trailing silence can push the last stamp
            // past the reported duration. Monotonic — never step backwards.
            let frac = ((end / total_seconds) as f32).clamp(0.0, 0.99);
            if frac > last_frac {
                last_frac = frac;
                on_progress(frac);
            }
        }
    };

    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = child.kill();
            break;
        }
        match chunk_rx.recv_timeout(std::time::Duration::from_millis(150)) {
            Ok(chunk) => {
                stdout_full.extend_from_slice(&chunk);
                for &b in &chunk {
                    if b == b'\r' || b == b'\n' {
                        report(&line);
                        line.clear();
                    } else {
                        line.push(b);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if !cancelled {
        report(&line);
    }

    if let Some(h) = stdout_handle {
        let _ = h.join();
    }
    let stderr_full = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let status = child.wait()?;

    // Error diagnostics may land on either stream; the tail filter drops
    // progress lines, so concatenating the two is safe.
    let mut combined = stderr_full.clone();
    combined.extend_from_slice(&stdout_full);
    let combined_text = String::from_utf8_lossy(&combined).into_owned();

    // Opt-in raw capture of both streams, labelled, so the engine's exact
    // progress format can be inspected to tune the parser. Off unless
    // SHADOWING_DEBUG_STDERR is set; appends (transcribe + translate are two
    // runs) to a fixed temp file.
    if std::env::var_os("SHADOWING_DEBUG_STDERR").is_some() {
        use std::io::Write as _;
        let path = std::env::temp_dir().join("shadowing-whisper-stderr.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(b"----- STDOUT -----\n");
            let _ = f.write_all(&stdout_full);
            let _ = f.write_all(b"\n----- STDERR -----\n");
            let _ = f.write_all(&stderr_full);
            let _ = f.write_all(b"\n===== end of run =====\n");
        }
    }

    Ok((status, combined_text))
}

/// Best-effort total duration of an audio file, in seconds, via a header-only
/// ffmpeg probe. `ffmpeg -i <file>` with no output writes a
/// `Duration: HH:MM:SS.ss` line to stderr and exits non-zero — it reads the
/// container header, it does not decode. None if ffmpeg is unavailable or prints
/// no duration, in which case live progress is simply disabled.
fn probe_duration_seconds(input: &Path) -> Option<f64> {
    let output = no_window_command(crate::download::resolve_program("ffmpeg"))
        .args(["-hide_banner", "-i"])
        .arg(input)
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let idx = stderr.find("Duration:")?;
    let after = &stderr[idx + "Duration:".len()..];
    // "Duration: 00:16:23.45, start: ..." — take up to the comma.
    let ts = after.split(',').next()?.trim();
    clock_to_seconds(ts)
}

/// Parse a colon-separated clock into seconds, tolerant of 2- or 3-part forms:
/// `"MM:SS.mmm"` and `"HH:MM:SS.ss"` both work (the engine prints the former
/// for sub-hour audio, ffmpeg's Duration the latter). None on anything that
/// isn't purely numeric parts.
fn clock_to_seconds(ts: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in ts.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    Some(total)
}

/// The end time (in seconds) of a segment line the engine streams while
/// transcribing — `[00:05.220 --> 00:10.960]  text` yields 10.96. None for any
/// line without the `-->` marker (status/blank/summary lines), so only real
/// segment progress moves the bar.
fn parse_segment_end_seconds(line: &str) -> Option<f64> {
    let arrow = line.find("-->")?;
    let rest = line[arrow + 3..].trim_start();
    // The end stamp runs up to the closing ']' (or whitespace before the text).
    let end = rest.split(']').next().unwrap_or(rest);
    let end = end.split_whitespace().next()?;
    clock_to_seconds(end)
}

/// The last `n` *meaningful* lines of a tool's stderr — the ones that might name
/// an actual error — with progress bars and blank lines removed.
///
/// tqdm draws progress by rewriting a line with carriage returns, so a captured
/// stderr is one long run of "model.bin: 47%|####…, 20 MB/s" fragments. Taking a
/// naive tail of that shows the user nothing but download noise, which is
/// exactly how a real error stayed invisible. Split on both \r and \n so those
/// fragments separate, then drop anything that looks like progress.
fn meaningful_stderr_tail(stderr: &str, n: usize) -> String {
    let lines: Vec<&str> = stderr
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            // A tqdm bar has a "NN%|" meter or a rate suffix; drop both.
            !(l.contains("%|") || l.contains("B/s]") || l.contains("it/s]"))
        })
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// whisperx (Python module) backend — the original path, unchanged, used
/// when the standalone engine hasn't been downloaded.
fn transcribe_with_whisperx(
    input: &Path,
    model: WhisperModel,
    task: Task,
    // whisperx runs as a single blocking call, so cancellation can only be
    // honoured before it starts — once launched it runs to completion.
    cancel: &AtomicBool,
    // whisperx runs with TQDM_DISABLE set (so its progress bars don't bury real
    // errors), so there's no live percentage to stream — this path stays on the
    // time-based estimate. Accepted to keep one transcription signature.
    _on_progress: &dyn Fn(f32),
) -> Result<Vec<Segment>, BoxErr> {
    if cancel.load(Ordering::Relaxed) {
        return Err("transcription cancelled".into());
    }
    let work_dir = unique_work_dir("whisperx");
    std::fs::create_dir_all(&work_dir)?;

    let (program, prefix_args) =
        find_module_invocation("whisperx", "whisperx").ok_or_else(|| -> BoxErr {
            "whisperx not installed. Install with: pip install --user whisperx \
             (see https://github.com/m-bain/whisperX for CUDA setup)"
                .into()
        })?;

    let device = pick_torch_device();
    let compute_type = if device == "cuda" { "float16" } else { "int8" };

    let mut args: Vec<String> = Vec::new();
    args.extend(prefix_args.into_iter());
    args.push(input.to_string_lossy().into_owned());
    for s in [
        "--model",
        model.whisperx_value(),
        // transcribe (source language) or translate (to English).
        "--task",
        task.whisper_value(),
        "--device",
        device.as_str(),
        "--compute_type",
        compute_type,
        "--output_format",
        "json",
        "--output_dir",
    ] {
        args.push(s.to_string());
    }
    args.push(work_dir.to_string_lossy().into_owned());
    for s in ["--segment_resolution", "sentence", "--vad_method", "silero"] {
        args.push(s.to_string());
    }

    let output = utf8_output(&mut no_window_command(&program))
        .env("TQDM_DISABLE", "1")
        .args(&args)
        .output()
        .map_err(|e| -> BoxErr { format!("failed to launch whisperx: {e}").into() })?;

    if !output.status.success() {
        // A whisperx that launches but cannot transcribe is worse than one
        // that is missing: the start-up probe is a `--version` call, so it
        // passes, the app believes it has an engine, and it never offers the
        // standalone download that would fix things. Record the failure so
        // the next probe knows better.
        crate::deps::mark_whisperx_unusable();

        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = tool_error_detail(&stderr, 4);
        if detail.is_empty() {
            return Err(format!("whisperx failed ({})", output.status).into());
        }
        return Err(format!("whisperx failed: {detail}").into());
    }

    // WhisperX writes <input_stem>.json into the output dir.
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    let json_path = work_dir.join(format!("{stem}.json"));
    let json_text = std::fs::read_to_string(&json_path).map_err(|e| -> BoxErr {
        format!(
            "whisperx ran but no JSON found at {}: {e}",
            json_path.display()
        )
        .into()
    })?;

    let segments = parse_whisperx_segments(&json_text)?;
    let _ = std::fs::remove_dir_all(&work_dir);

    if segments.is_empty() {
        return Err("whisperx returned no segments".into());
    }
    Ok(segments)
}

/// The part of a failed tool's output worth putting in front of the user.
///
/// A plain tail of stderr is the obvious thing and the wrong thing for the
/// Python tools. Their tracebacks do end with the exception and its message,
/// but the lines immediately above it are the echoed source line and a row of
/// `^^^^` carets - so a tail leads with carets and buries the one sentence
/// that says what broke ("Library cublas64_12.dll is not found", say). Lead
/// with the exception instead, keeping `context` lines after it: the job row
/// shows the first line, and the whole string on hover.
///
/// Falls back to the last `context` lines when nothing looks like an
/// exception, which is the right answer for ffmpeg and yt-dlp.
fn tool_error_detail(text: &str, context: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        // Progress leftovers and caret markers say nothing about the failure.
        .filter(|l| !l.contains("MB/s") && !l.contains("kB/s"))
        .filter(|l| !l.trim().chars().all(|c| c == '^'))
        .collect();

    if lines.is_empty() {
        return String::new();
    }

    match lines.iter().rposition(|l| is_exception_line(l)) {
        Some(i) => {
            let mut out = Vec::with_capacity(context + 1);
            out.push(lines[i]);
            out.extend_from_slice(&lines[i.saturating_sub(context)..i]);
            out.join("\n")
        }
        None => lines[lines.len().saturating_sub(context.max(1))..].join("\n"),
    }
}

/// Does this line look like the last line of a Python traceback - an
/// exception type, a colon, then a message?
///
/// Deliberately narrow. A Windows path ("C:\\Users\\...") and an ordinary log line
/// ("warning: ...") both contain a colon, and neither is the failure.
fn is_exception_line(line: &str) -> bool {
    let Some((head, message)) = line.split_once(':') else {
        return false;
    };
    if message.trim().is_empty() {
        return false;
    }
    let head = head.trim();
    if head.contains(char::is_whitespace) {
        return false;
    }
    // Custom exceptions arrive fully qualified: `whisperx.asr.ModelError`.
    let name = head.rsplit('.').next().unwrap_or(head);
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && (name.ends_with("Error")
            || name.ends_with("Exception")
            || name.ends_with("Interrupt"))
}

#[cfg(test)]
mod tool_error_tests {
    use super::{is_exception_line, tool_error_detail};

    /// The case that prompted all this: the real message is the last line, and
    /// a naive tail would have surfaced the carets above it instead.
    #[test]
    fn leads_with_the_exception_not_the_carets() {
        let stderr = "\
Using cache found in C:\\Users\\Henry/.cache\\torch\\hub
Traceback (most recent call last):
  File \"whisperx\\asr.py\", line 104, in encode
    return self.model.encode(features, to_cpu=to_cpu)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
RuntimeError: Library cublas64_12.dll is not found or cannot be loaded";

        let detail = tool_error_detail(stderr, 4);
        assert_eq!(
            detail.lines().next().unwrap(),
            "RuntimeError: Library cublas64_12.dll is not found or cannot be loaded"
        );
        // Context is kept for the hover text, minus the caret line.
        assert!(detail.contains("line 104, in encode"));
        assert!(!detail.contains("^^^"));
    }

    #[test]
    fn falls_back_to_the_tail_for_non_python_tools() {
        let stderr = "\
[libx264 @ 000001] using cpu capabilities
Conversion failed!
Output file is empty, nothing was encoded";

        let detail = tool_error_detail(stderr, 2);
        assert_eq!(
            detail,
            "Conversion failed!\nOutput file is empty, nothing was encoded"
        );
    }

    #[test]
    fn empty_output_gives_an_empty_detail() {
        assert_eq!(tool_error_detail("", 4), "");
        assert_eq!(tool_error_detail("   \n\n  \n", 4), "");
    }

    #[test]
    fn only_real_exception_lines_count() {
        assert!(is_exception_line("RuntimeError: boom"));
        assert!(is_exception_line("ValueError: bad value"));
        assert!(is_exception_line("whisperx.asr.ModelError: no model"));
        // A drive letter, a log level, and a bare type with no message.
        assert!(!is_exception_line("C:\\Users\\Henry\\clip.wav"));
        assert!(!is_exception_line("warning: something happened"));
        assert!(!is_exception_line("RuntimeError:"));
        assert!(!is_exception_line("Traceback (most recent call last):"));
    }
}

/// Parse an SRT subtitle file into `Segment`s. SRT blocks are separated by
/// blank lines; each has an index line, a `HH:MM:SS,mmm --> HH:MM:SS,mmm`
/// timing line, then one or more text lines. Tolerant of CRLF (the engine
/// is a Windows exe), a missing leading index, and `.`-vs-`,` millisecond
/// separators.
fn parse_srt_segments(srt: &str) -> Vec<Segment> {
    let normalized = srt.replace("\r\n", "\n").replace('\r', "\n");
    let mut segments = Vec::new();

    for block in normalized.split("\n\n") {
        let lines: Vec<&str> = block
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        // Timing line is whichever line contains "-->"; text is everything
        // after it (a lone index line before it is ignored).
        let Some(arrow_idx) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let timing = lines[arrow_idx];
        let Some((start_s, end_s)) = timing.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (
            parse_srt_timestamp(start_s.trim()),
            parse_srt_timestamp(end_s.trim()),
        ) else {
            continue;
        };
        let text = lines[arrow_idx + 1..].join(" ").trim().to_string();
        if text.is_empty() {
            continue;
        }
        // SRT has no way to express word timings; they're attached afterwards
        // from the JSON written by the same run (see transcribe_with_standalone).
        segments.push(Segment {
            start,
            end,
            text,
            words: Vec::new(),
            translation: None,
        });
    }
    segments
}

/// `HH:MM:SS,mmm` (or `HH:MM:SS.mmm`) → seconds. Returns None on anything
/// that doesn't look like a timestamp.
fn parse_srt_timestamp(ts: &str) -> Option<f64> {
    let ts = ts.replace(',', ".");
    let (hms, millis) = match ts.split_once('.') {
        Some((h, m)) => (h, m),
        None => (ts.as_str(), "0"),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    // Pad/truncate millis to 3 digits so "5" -> 500ms, not 5ms.
    let mut ms_str = millis.to_string();
    while ms_str.len() < 3 {
        ms_str.push('0');
    }
    ms_str.truncate(3);
    let ms: f64 = ms_str.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec + ms / 1000.0)
}

// ---------------------------------------------------------------------------
// whisper.cpp
// ---------------------------------------------------------------------------

/// Whether the whisper.cpp engine is on hand at all.
///
/// It ships inside the app, so this is normally true; it answers false in a
/// dev build run straight out of `target/`, where there is no bundle to carry
/// it. The model prompts key off this, because offering to download a model
/// for an engine that is not there would be a dead end.
pub fn whispercpp_available() -> bool {
    crate::download::resolve_program("whisper-cli").is_file()
}

/// Whether `model` can be used right now without downloading anything.
pub fn model_ready(model: WhisperModel) -> bool {
    whispercpp_model_path(model).is_some()
}

/// Whether whisper.cpp could transcribe something right now: the engine is
/// here, and so is at least one model.
///
/// "At least one" rather than the selected one on purpose. This answers the
/// dependency question — is there an engine at all — and the smallest model
/// ships with the app, so on a normal install it is yes. Whether the model the
/// user has *chosen* needs fetching is a different question, asked and answered
/// by the download button beside the accuracy picker.
pub fn whispercpp_ready() -> bool {
    whispercpp_available() && WhisperModel::ALL.iter().any(|m| model_ready(*m))
}

/// Where the GGML file for `model` is, if we have it.
///
/// Two places, in order: bundled inside the app for the one model that ships
/// with it, then the managed directory for anything the user chose to download
/// afterwards. Same shape as `resolve_program` - bundled first, downloaded
/// second - so a shipped model can never be shadowed by a half-finished one.
fn whispercpp_model_path(model: WhisperModel) -> Option<PathBuf> {
    let file = model.ggml_file();

    if let Some(bundled) = crate::download::bundled_model_dir() {
        let candidate = bundled.join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    crate::download::installed_exe(crate::download::ToolId::WhisperModel(model))
}

/// whisper.cpp backend. The reason it exists: it reaches the GPU through Metal,
/// where the standalone engine is an x86-64 build running on the CPU under
/// Rosetta. Measured on an M1 Max against the same clip, `small` went from 2.2
/// audio-seconds/s to 17 - and 17 is nearly double what the *tiny* model
/// managed on the old path, so it buys accuracy and speed at once.
fn transcribe_with_whispercpp(
    engine: &Path,
    model_file: &Path,
    input: &Path,
    model: WhisperModel,
    task: Task,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f32),
) -> Result<Vec<Segment>, BoxErr> {
    let work_dir = unique_work_dir("wcpp");
    std::fs::create_dir_all(&work_dir)?;

    // whisper.cpp reads 16 kHz mono 16-bit WAV and nothing else - it does no
    // demuxing and no resampling of its own. Every other backend takes whatever
    // format the source happened to be, so convert here rather than making the
    // caller care which engine it is talking to.
    let wav = work_dir.join("audio16k.wav");
    let convert = no_window_command(crate::download::resolve_program("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&wav)
        .status()
        .map_err(|e| -> BoxErr { format!("failed to launch ffmpeg: {e}").into() })?;
    if !convert.success() || !wav.is_file() {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err("could not convert the audio to 16 kHz mono for whisper.cpp".into());
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err("job cancelled".into());
    }

    let out_stem = work_dir.join("out");
    let mut cmd = no_window_command(engine);
    cmd.arg("-m")
        .arg(model_file)
        .arg("-f")
        .arg(&wav)
        // Auto-detect. whisper.cpp defaults to English rather than detecting,
        // which would quietly transcribe every other language as gibberish.
        .args(["-l", "auto"])
        // The machine has cores the default does not use. Physical cores only:
        // counting the efficiency ones in slows this down rather than up.
        .args(["-t", &whispercpp_threads().to_string()])
        // Token spans, and the DTW pass that makes them worth having. Without
        // --dtw the tokens carry the decoder's guess rather than an alignment.
        .arg("-ojf")
        .args(["-dtw", model.whispercpp_dtw_value()])
        .arg("-of")
        .arg(&out_stem);
    if matches!(task, Task::Translate) {
        cmd.arg("-tr");
    }

    let total_seconds = probe_duration_seconds(input).unwrap_or(0.0);
    let (status, stderr_text) = run_engine_streaming(cmd, total_seconds, cancel, on_progress)
        .map_err(|e| -> BoxErr { format!("failed to launch whisper.cpp: {e}").into() })?;

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err("job cancelled".into());
    }
    if !status.success() {
        let detail = tool_error_detail(&stderr_text, 4);
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err(format!("whisper.cpp failed ({status}). {detail}").into());
    }

    // -of gives a stem; whisper.cpp appends the extension itself.
    let json_path = out_stem.with_extension("json");
    let json = std::fs::read_to_string(&json_path).map_err(|e| -> BoxErr {
        format!(
            "whisper.cpp wrote no JSON at {}: {e}",
            json_path.display()
        )
        .into()
    })?;
    let segments = parse_whispercpp_segments(&json);
    let _ = std::fs::remove_dir_all(&work_dir);
    segments
}

/// Threads to give whisper.cpp: the performance cores, and no more.
///
/// The efficiency cores are much slower, and whisper.cpp splits work evenly
/// rather than by core speed, so including them means every batch waits on the
/// slowest worker. Falls back to the total when the split is not reported.
fn whispercpp_threads() -> usize {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = no_window_command("sysctl")
            .args(["-n", "hw.perflevel0.logicalcpu"])
            .output()
        {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<usize>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// whisper.cpp's `--output-json-full` shape, as far as we care about it.
///
/// Nothing like the other two backends: the timings are integer milliseconds
/// rather than float seconds, they live under `offsets`, and the sub-segment
/// unit is a *token* rather than a word. Hence its own structs and its own
/// parser rather than a widened `WhisperJson`.
#[derive(serde::Deserialize)]
struct WhisperCppJson {
    #[serde(default)]
    transcription: Vec<WhisperCppSegment>,
}

#[derive(serde::Deserialize)]
struct WhisperCppSegment {
    offsets: WhisperCppOffsets,
    #[serde(default)]
    text: String,
    /// Only present with `--output-json-full`, and only carries useful timings
    /// with `--dtw`. Absent is not an error: the splitter falls back to cutting
    /// on silence when a segment has no words.
    #[serde(default)]
    tokens: Vec<WhisperCppToken>,
}

#[derive(serde::Deserialize)]
struct WhisperCppOffsets {
    from: i64,
    to: i64,
}

#[derive(serde::Deserialize)]
struct WhisperCppToken {
    #[serde(default)]
    text: String,
    offsets: WhisperCppOffsets,
}

/// Rebuild whole words out of whisper.cpp's tokens.
///
/// The model emits sub-word pieces - "cru", "z", "adas" - so a token is not a
/// word. What marks a word boundary is a *leading space* on the token text,
/// which is how the tokenizer encodes "a new word starts here". So: start a new
/// word on a token that begins with a space, otherwise glue this token onto the
/// one being built.
///
/// Special tokens (`[_BEG_]`, `[_TT_180]`) are markers, not speech. They carry
/// no useful span and would otherwise show up as literal text in a subtitle.
fn words_from_tokens(tokens: &[WhisperCppToken]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();

    for tok in tokens {
        if tok.text.starts_with('[') && tok.text.ends_with(']') {
            continue;
        }
        let start = tok.offsets.from as f64 / 1000.0;
        let end = tok.offsets.to as f64 / 1000.0;

        match words.last_mut() {
            // Continuation of the word in progress: extend its span and text.
            Some(w) if !tok.text.starts_with(' ') => {
                w.text.push_str(&tok.text);
                w.end = w.end.max(end);
            }
            _ => words.push(Word {
                start,
                end,
                text: tok.text.trim_start().to_string(),
            }),
        }
    }

    // A token whose span the model never pinned down comes back as zero width.
    // Dropping empties keeps those out of the word gaps the splitter measures.
    words.retain(|w| !w.text.trim().is_empty());
    words
}

/// Parse `--output-json-full` into the segments the rest of the pipeline wants.
fn parse_whispercpp_segments(json: &str) -> Result<Vec<Segment>, BoxErr> {
    let parsed: WhisperCppJson = serde_json::from_str(json)
        .map_err(|e| -> BoxErr { format!("failed to parse whisper.cpp JSON: {e}").into() })?;

    Ok(parsed
        .transcription
        .into_iter()
        .map(|s| Segment {
            start: s.offsets.from as f64 / 1000.0,
            end: s.offsets.to as f64 / 1000.0,
            text: s.text.trim().to_string(),
            words: words_from_tokens(&s.tokens),
            translation: None,
        })
        .filter(|s| !s.text.is_empty())
        .collect())
}

#[cfg(test)]
mod duration_cap_tests {
    use super::*;

    fn seg(start: f64, end: f64, words: &[(f64, f64)]) -> Segment {
        Segment {
            start,
            end,
            text: "x".into(),
            words: words
                .iter()
                .map(|(s, e)| Word {
                    start: *s,
                    end: *e,
                    text: "w".into(),
                })
                .collect(),
            translation: None,
        }
    }

    #[test]
    fn segments_past_the_limit_are_dropped() {
        let mut segs = vec![seg(0.0, 5.0, &[]), seg(301.0, 306.0, &[])];
        clamp_segments_to(&mut segs, 300.0);
        assert_eq!(segs.len(), 1);
    }

    /// The shape that actually broke a job: a segment straddling the 5-minute
    /// cap, with words running well past it.
    #[test]
    fn a_straddling_segment_is_clamped_words_and_all() {
        let mut segs = vec![seg(297.3, 307.0, &[(297.3, 298.0), (305.9, 306.97)])];
        clamp_segments_to(&mut segs, 300.0);
        assert_eq!(segs[0].end, 300.0);
        assert!(
            segs[0].words.iter().all(|w| w.end <= 300.0),
            "no word may point past the audio that was kept"
        );
    }

    #[test]
    fn an_inverted_cut_pair_is_skipped_not_panicked() {
        let samples = vec![0i16; 1000];
        let s = seg(0.0, 1.0, &[]);
        // start after end - exactly what the split log recorded before the
        // job thread died.
        let chunks = cut_times_to_chunks(&samples, &s, &[100, 900, 500]);
        assert_eq!(chunks.len(), 1, "the good pair survives, the bad one goes");
    }

    #[test]
    fn cuts_past_the_buffer_are_clamped_not_panicked() {
        let samples = vec![0i16; 1000];
        let s = seg(0.0, 1.0, &[]);
        let chunks = cut_times_to_chunks(&samples, &s, &[0, 5000]);
        assert_eq!(chunks[0].audio.len(), 1000);
    }
}

#[cfg(test)]
mod whispercpp_json_tests {
    use super::*;

    /// Trimmed from a real `--output-json-full --dtw small` run, keeping the
    /// awkward parts: a `[_BEG_]` marker, and "cruzadas" arriving as three
    /// separate pieces.
    const SAMPLE: &str = r#"{
      "transcription": [
        {
          "offsets": { "from": 0, "to": 6340 },
          "text": " las cruzadas fueron",
          "tokens": [
            { "text": "[_BEG_]", "offsets": { "from": 0, "to": 0 } },
            { "text": " las",     "offsets": { "from": 0,   "to": 140 } },
            { "text": " cru",     "offsets": { "from": 190, "to": 320 } },
            { "text": "z",        "offsets": { "from": 320, "to": 320 } },
            { "text": "adas",     "offsets": { "from": 410, "to": 590 } },
            { "text": " fueron",  "offsets": { "from": 590, "to": 920 } }
          ]
        }
      ]
    }"#;

    #[test]
    fn milliseconds_become_seconds() {
        let segs = parse_whispercpp_segments(SAMPLE).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start, 0.0);
        assert_eq!(segs[0].end, 6.34);
    }

    #[test]
    fn sub_word_tokens_are_glued_back_into_words() {
        let segs = parse_whispercpp_segments(SAMPLE).unwrap();
        let words: Vec<&str> = segs[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, ["las", "cruzadas", "fueron"]);
    }

    #[test]
    fn a_glued_word_spans_all_of_its_pieces() {
        let segs = parse_whispercpp_segments(SAMPLE).unwrap();
        let cruzadas = &segs[0].words[1];
        assert_eq!(cruzadas.start, 0.19);
        assert_eq!(cruzadas.end, 0.59);
    }

    #[test]
    fn marker_tokens_never_reach_the_transcript() {
        let segs = parse_whispercpp_segments(SAMPLE).unwrap();
        assert!(!segs[0].words.iter().any(|w| w.text.contains("_BEG_")));
    }

    #[test]
    fn a_segment_without_tokens_still_parses() {
        let json = r#"{"transcription":[{"offsets":{"from":500,"to":1500},"text":"hola"}]}"#;
        let segs = parse_whispercpp_segments(json).unwrap();
        assert_eq!(segs[0].text, "hola");
        assert!(segs[0].words.is_empty(), "no words is valid, not an error");
    }

    #[test]
    fn empty_transcription_is_not_an_error() {
        assert!(parse_whispercpp_segments(r#"{"transcription":[]}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_whispercpp_segments("not json").is_err());
    }
}

/// Whisper's JSON, as far as we care about it. Both backends emit this shape:
/// segments, each optionally carrying word timings. Every field is optional
/// because word timings are an enhancement — a file without them still parses.
#[derive(serde::Deserialize)]
struct WhisperJson {
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(serde::Deserialize)]
struct RawSegment {
    start: f64,
    end: f64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    words: Vec<RawWord>,
}

#[derive(serde::Deserialize)]
struct RawWord {
    /// Present on aligned words; missing ones are skipped (whisperx leaves
    /// timings off words it couldn't align).
    start: Option<f64>,
    end: Option<f64>,
    /// whisperx and faster-whisper both call it "word"; accept "text" too.
    #[serde(alias = "text")]
    word: Option<String>,
}

impl RawWord {
    fn into_word(self) -> Option<Word> {
        let text = self.word?.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let (start, end) = (self.start?, self.end?);
        if end < start {
            return None;
        }
        Some(Word { start, end, text })
    }
}

/// Parse WhisperX's JSON into segments, with word timings when it aligned them.
fn parse_whisperx_segments(json: &str) -> Result<Vec<Segment>, BoxErr> {
    let parsed: WhisperJson = serde_json::from_str(json)
        .map_err(|e| -> BoxErr { format!("failed to parse whisperx JSON: {e}").into() })?;

    let segments = parsed
        .segments
        .into_iter()
        .map(|s| Segment {
            start: s.start,
            end: s.end,
            text: s.text.trim().to_string(),
            words: s.words.into_iter().filter_map(RawWord::into_word).collect(),
            translation: None,
        })
        .collect();

    Ok(segments)
}

/// Every word in a Whisper JSON file, flattened and in time order, ignoring how
/// that file chose to group them into segments. Used for the standalone engine,
/// whose JSON segments are *not* the sentence-split ones we want — we take only
/// the word timings from it and keep the SRT's sentences. Best-effort: an
/// unparseable or word-less file yields an empty list.
fn parse_json_words(json: &str) -> Vec<Word> {
    let Ok(parsed) = serde_json::from_str::<WhisperJson>(json) else {
        return Vec::new();
    };
    let mut words: Vec<Word> = parsed
        .segments
        .into_iter()
        .flat_map(|s| s.words)
        .filter_map(RawWord::into_word)
        .collect();
    words.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    words
}

/// Distribute a flat word list across segments by time. A word belongs to the
/// segment it overlaps most — using the midpoint keeps a word that straddles a
/// boundary from landing in both.
fn attach_words(segments: &mut [Segment], words: &[Word]) {
    for segment in segments.iter_mut() {
        segment.words = words
            .iter()
            .filter(|w| {
                let mid = (w.start + w.end) / 2.0;
                mid >= segment.start && mid < segment.end
            })
            .cloned()
            .collect();
    }
}

/// Distribute English-translation segments across the original segments by
/// time. Whisper's translate pass has its own sentence boundaries that don't
/// line up 1:1 with the transcribe pass, so each translated segment is filed
/// under the original it overlaps most (falling back to the nearest by midpoint
/// when nothing overlaps at all). Segments with no translated text stay None.
fn attach_translations(segments: &mut [Segment], english: &[Segment]) {
    if segments.is_empty() {
        return;
    }

    let mut buckets: Vec<Vec<&str>> = vec![Vec::new(); segments.len()];
    for e in english {
        let text = e.text.trim();
        if text.is_empty() {
            continue;
        }

        // The original segment sharing the most time with this translation.
        let mut best: Option<(usize, f64)> = None;
        for (i, s) in segments.iter().enumerate() {
            let overlap = (e.end.min(s.end) - e.start.max(s.start)).max(0.0);
            if overlap > 0.0 && best.map_or(true, |(_, b)| overlap > b) {
                best = Some((i, overlap));
            }
        }

        // No overlap anywhere (disjoint timelines): fall back to the segment
        // whose midpoint is nearest, so no translated text is silently dropped.
        let idx = best.map(|(i, _)| i).unwrap_or_else(|| {
            let e_mid = (e.start + e.end) / 2.0;
            segments
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = ((a.start + a.end) / 2.0 - e_mid).abs();
                    let db = ((b.start + b.end) / 2.0 - e_mid).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
                .unwrap_or(0)
        });
        buckets[idx].push(text);
    }

    for (seg, bucket) in segments.iter_mut().zip(buckets) {
        if !bucket.is_empty() {
            seg.translation = Some(bucket.join(" "));
        }
    }
}

/// A unit of audio to be repeated, plus the transcript text it corresponds
/// to (for optional on-screen subtitles). `text` may be empty (e.g. for the
/// second half of a chunk that was split at a silence). `translation` is the
/// English rendering of `text` when the user enabled it, else empty.
fn find_start_and_end(i: usize, segments: &[Segment], samples: &[i16]) -> Option<(usize, usize)> {
    const MAX_SILENCE_EACH_SIDE: f64 = 0.5;
    const SHIFT: f64 = 0.300;

    let sample_rate = SAMPLE_RATE as f64;
    let seg = &segments[i];

    let start_time = if i == 0 {
        (seg.start - MAX_SILENCE_EACH_SIDE).max(0.0)
    } else {
        let previous = &segments[i - 1];
        let gap = seg.start - previous.end;

        if gap >= 1.0 {
            seg.start - MAX_SILENCE_EACH_SIDE
        } else {
            quietest_cut_time(samples, previous.end + SHIFT, seg.start + SHIFT)
        }
    };

    let end_time = if i + 1 == segments.len() {
        seg.end + MAX_SILENCE_EACH_SIDE
    } else {
        let next = &segments[i + 1];
        let gap = next.start - seg.end;

        if gap >= 1.0 {
            seg.end + MAX_SILENCE_EACH_SIDE
        } else {
            quietest_cut_time(samples, seg.end + 0.2, next.start + 0.7)
        }
    };

    let start = ((start_time.max(0.0) * sample_rate) as usize).min(samples.len());

    let end = ((end_time.max(0.0) * sample_rate) as usize).min(samples.len());

    if end <= start {
        return None;
    }

    Some((start, end))
}

struct Chunk<'a> {
    audio: &'a [i16],
    text: String,
    translation: String,
}

fn segments_to_chunks<'a>(
    samples: &'a [i16],
    segments: &[Segment],
    max_chunk_seconds: f64,
    split_log: &mut BufWriter<std::fs::File>,
    on_progress: &dyn Fn(f32),
) -> Result<Vec<Chunk<'a>>, std::io::Error> {
    let sample_rate = SAMPLE_RATE as f64;
    let mut chunks: Vec<Chunk<'a>> = Vec::new();
    let segment_count = segments.len();

    for (i, seg) in segments.iter().enumerate() {
        // Real progress for the splitting stage: how many segments are done.
        // Cheap and exact, unlike the time-based estimate it replaces.
        if segment_count > 0 {
            on_progress(i as f32 / segment_count as f32);
        }
        writeln!(split_log, "\nSEGMENT {}", i + 1)?;
        writeln!(
            split_log,
            "Whisper segment: {:.3} -> {:.3}",
            seg.start, seg.end
        )?;
        writeln!(split_log, "Text: {}", seg.text)?;
        writeln!(split_log, "Words: {}", seg.words.len())?;
        for (word_index, word) in seg.words.iter().enumerate() {
            writeln!(
                split_log,
                "  Word {}: {:.3} -> {:.3}  {:?}",
                word_index + 1,
                word.start,
                word.end,
                word.text
            )?;
        }

        let Some((start_sample, end_sample)) = find_start_and_end(i, segments, samples) else {
            writeln!(split_log, "find_start_and_end returned no valid range")?;
            writeln!(split_log)?;
            continue;
        };

        let start_seconds = start_sample as f64 / sample_rate;
        let end_seconds = end_sample as f64 / sample_rate;

        writeln!(split_log, "After find_start_and_end:")?;
        writeln!(
            split_log,
            "  Start: {:.3} seconds -> sample {}",
            start_seconds, start_sample
        )?;
        writeln!(
            split_log,
            "  End:   {:.3} seconds -> sample {}",
            end_seconds, end_sample
        )?;

        let segment_length = seg.end - seg.start;

        writeln!(
            split_log,
            "Whisper segment length: {:.3} seconds",
            segment_length
        )?;

        if segment_length <= max_chunk_seconds * 1.1 {
            writeln!(split_log, "Entire segment is short enough")?;
            writeln!(split_log)?;

            chunks.extend(cut_times_to_chunks(
                samples,
                seg,
                &[start_sample, end_sample],
            ));
            continue;
        }

        writeln!(split_log, "Segment is too long and must be split")?;

        let mut times_of_words_after_pauses: Vec<f64> = Vec::new();

        writeln!(split_log, "Punctuation pause candidates:")?;

        for (word_index, word) in seg.words.iter().enumerate() {
            if word_index + 1 == seg.words.len() {
                continue;
            }

            let ending_char = word.text.trim().chars().last();

            if matches!(ending_char, Some(',' | ';' | ':' | '—')) {
                let pause_time = seg.words[word_index + 1].start;

                times_of_words_after_pauses.push(pause_time);

                writeln!(
                    split_log,
                    "  After {:?}: candidate cut at {:.3} seconds",
                    word.text, pause_time
                )?;
            }
        }

        if times_of_words_after_pauses.is_empty() {
            writeln!(split_log, "  None found")?;
        }

        let segment_length = seg.end - seg.start;

        let middle_50_start = seg.start + (segment_length * 0.0);

        let middle_50_end = seg.start + (segment_length * 1.0);

        let segment_centre = seg.start + segment_length * 0.50;

        writeln!(
            split_log,
            "Middle 50% range: {:.3} -> {:.3}",
            middle_50_start, middle_50_end
        )?;

        writeln!(split_log, "Segment centre: {:.3}", segment_centre)?;

        let mut closest_pause_time: Option<f64> = None;
        let mut closest_distance = f64::INFINITY;

        for pause_time in &times_of_words_after_pauses {
            if *pause_time >= middle_50_start && *pause_time <= middle_50_end {
                let distance = (*pause_time - segment_centre).abs();

                if distance < closest_distance {
                    closest_distance = distance;
                    closest_pause_time = Some(*pause_time);
                }
            }
        }

        match closest_pause_time {
            Some(pause_time) => {
                writeln!(
                    split_log,
                    "Selected central punctuation candidate: {:.3} seconds",
                    pause_time
                )?;
                writeln!(
                    split_log,
                    "Distance from segment centre: {:.3} seconds",
                    closest_distance
                )?;
            }
            None => {
                writeln!(
                    split_log,
                    "No punctuation candidate found inside the middle 50%"
                )?;
            }
        }

        if let Some(pause_time) = closest_pause_time {
            let search_start = pause_time - 0.200;
            let search_end = pause_time + 0.300;

            writeln!(
                split_log,
                "Refining punctuation candidate {:.3}:",
                pause_time
            )?;

            writeln!(
                split_log,
                "  quietest_cut_time search range: {:.3} -> {:.3}",
                search_start, search_end
            )?;

            let cut_time = quietest_cut_time(samples, search_start, search_end);

            writeln!(split_log, "  Refined cut time: {:.3} seconds", cut_time)?;

            let cut_sample = (cut_time * sample_rate) as usize;

            let first_piece_length = cut_time - seg.start;
            let second_piece_length = seg.end - cut_time;
            let longest_piece = first_piece_length.max(second_piece_length);
            let allowed_length = max_chunk_seconds * 1.2;

            writeln!(
                split_log,
                "  First piece length:  {:.3} seconds",
                first_piece_length
            )?;

            writeln!(
                split_log,
                "  Second piece length: {:.3} seconds",
                second_piece_length
            )?;

            writeln!(
                split_log,
                "  Allowed length:      {:.3} seconds",
                allowed_length
            )?;

            if longest_piece < allowed_length {
                writeln!(split_log, "  Decision: accept this punctuation split")?;

                writeln!(
                    split_log,
                    "  Final samples: {} -> {} -> {}",
                    start_sample, cut_sample, end_sample
                )?;

                writeln!(split_log)?;

                chunks.extend(cut_times_to_chunks(
                    samples,
                    seg,
                    &[start_sample, cut_sample, end_sample],
                ));

                continue;
            }

            writeln!(
                split_log,
                "  Decision: reject this split because one piece is still too long\nCUTTING IN HALF NOT POSSIBLE\n"
            )?;
        }

        //what to do if cutting in half isn't enough

        let ideal_splits: Vec<f64> = find_splits(seg, max_chunk_seconds);
        let number_of_chunks = ideal_splits.len() + 1;

        let sub_segment_length = segment_length / number_of_chunks as f64;
        let search_range = sub_segment_length / 4.0;

        writeln!(split_log, "Multi-split planning")?;
        writeln!(split_log, "Number of intended chunks: {}", number_of_chunks)?;
        writeln!(
            split_log,
            "   Ideal chunk length: {:.3} seconds",
            sub_segment_length
        )?;
        writeln!(split_log, "  Ideal cuts returned by find_splits:")?;
        for (split_index, split) in ideal_splits.iter().enumerate() {
            writeln!(
                split_log,
                "    Split {}: {:.3} seconds ",
                split_index + 1,
                split,
            )?;
        }

        let mut cut_times: Vec<Option<f64>> = vec![None; ideal_splits.len() + 2];
        cut_times[0] = Some(start_seconds);
        let last_index = cut_times.len() - 1;
        cut_times[last_index] = Some(end_seconds);

        for (split_index, split) in ideal_splits.iter().enumerate() {
            let mut closest_pause: Option<f64> = None;
            let mut closest_distance = f64::INFINITY;

            writeln!(
                split_log,
                "  Searching for punctuation near ideal split {} at {:.3}: within range {} - {}",
                split_index + 1,
                split,
                split - search_range,
                split + search_range,
            )?;

            for word_after_pause in times_of_words_after_pauses.iter().copied() {
                let distance = (word_after_pause - split).abs();
                if distance < search_range {
                    writeln!(
                        split_log,
                        "    Candidate: {:.3}, distance from ideal split: {:.3}",
                        word_after_pause, distance
                    )?;

                    if distance < closest_distance {
                        closest_distance = distance;
                        closest_pause = Some(word_after_pause);
                    }
                }
            }
            if let Some(pause) = closest_pause {
                writeln!(
                    split_log,
                    "    Selected punctuation cut: {:.3} seconds",
                    pause
                )?;

                writeln!(
                    split_log,
                    "    Ideal split was {:.3}. Punctuation pause found at {:.3}",
                    split, pause
                )?;

                cut_times[split_index + 1] = Some(pause);
            } else {
                writeln!(
                    split_log,
                    "    No punctuation candidate found for split: {:.3}",
                    split
                )?;
            }
        }

        for j in 0..ideal_splits.len() {
            let cut_index = j + 1;

            if cut_times[cut_index].is_some() {
                continue;
            }

            let ideal_split = ideal_splits[j];

            writeln!(
                split_log,
                "  Searching  near ideal split {} at {:.3}:",
                j + 1,
                ideal_split
            )?;

            let search_window = (0.2 * sample_rate) as usize;
            let step_size = (0.005 * sample_rate) as usize;

            let search_start_time: f64 = ideal_split - search_range;
            let search_end_time: f64 = ideal_split + search_range;

            writeln!(
                split_log,
                "    Search range: {:.3} -> {:.3}",
                search_start_time, search_end_time
            )?;

            let search_start_sample =
                ((search_start_time * sample_rate) as usize).min(samples.len());

            let search_end_sample = ((search_end_time * sample_rate) as usize).min(samples.len());

            let last_possible_start = search_end_sample - search_window;

            let mut range_start_sample = search_start_sample;
            let mut best_score = u64::MAX;
            let mut best_range_start = search_start_sample;

            while range_start_sample <= last_possible_start {
                let range_end_sample = range_start_sample + search_window;

                let score: u64 = samples[range_start_sample..range_end_sample]
                    .iter()
                    .map(|sample| (*sample as i32).unsigned_abs() as u64)
                    .sum();

                writeln!(
                    split_log,
                    "    Range {:.3} -> {:.3} seconds, score: {}",
                    range_start_sample as f64 / sample_rate,
                    range_end_sample as f64 / sample_rate,
                    score
                )?;

                if score < best_score {
                    best_score = score;
                    best_range_start = range_start_sample;

                    writeln!(split_log, "      New best score: {}", best_score)?;
                }
                range_start_sample += step_size;
            }

            let best_range_end = best_range_start + search_window;

            let cut_sample = best_range_start + (best_range_end - best_range_start) / 2;

            let cut_time = cut_sample as f64 / sample_rate;

            writeln!(
                split_log,
                "    Quietest range: {:.3} -> {:.3} seconds",
                best_range_start as f64 / sample_rate,
                best_range_end as f64 / sample_rate
            )?;

            writeln!(split_log, "    Winning score: {}", best_score)?;

            writeln!(
                split_log,
                "    Preliminary cut time: {:.3} seconds",
                cut_time
            )?;

            cut_times[cut_index] = Some(cut_time);
        }

        let mut refined_cut_times = Vec::new();
        let last_index = cut_times.len() - 1;

        writeln!(split_log, "Final quietest_cut_time refinement:")?;

        for (cut_index, cut_time) in cut_times.into_iter().enumerate() {
            let cut_time = cut_time.expect("every cut time should be filled");

            if cut_index == 0 || cut_index == last_index {
                writeln!(
                    split_log,
                    "  Cut {}: {:.3} unchanged because it is an outer boundary",
                    cut_index, cut_time
                )?;

                refined_cut_times.push(cut_time);
                continue;
            }

            let search_start = cut_time - 0.3;
            let search_end = cut_time + 0.3;

            let refined_cut_time = quietest_cut_time(samples, search_start, search_end);

            writeln!(split_log, "  Cut {}:", cut_index)?;

            writeln!(split_log, "    Before quietest_cut_time: {:.3}", cut_time)?;

            writeln!(
                split_log,
                "    Search range: {:.3} -> {:.3}",
                search_start, search_end
            )?;

            writeln!(
                split_log,
                "    After quietest_cut_time:  {:.3}",
                refined_cut_time
            )?;

            writeln!(
                split_log,
                "    Change: {:+.3} seconds",
                refined_cut_time - cut_time
            )?;

            refined_cut_times.push(refined_cut_time);
        }

        let mut cut_samples: Vec<usize> = Vec::with_capacity(refined_cut_times.len());

        for cut_time in refined_cut_times {
            let cut_sample = ((cut_time * sample_rate) as usize).min(samples.len());

            cut_samples.push(cut_sample);
        }

        chunks.extend(cut_times_to_chunks(samples, seg, &cut_samples));
    }

    on_progress(1.0);
    Ok(chunks)
}

fn find_splits(seg: &Segment, max_chunk_seconds: f64) -> Vec<f64> {
    let seg_length: f64 = seg.end - seg.start;
    let num_segs: usize = (seg_length / max_chunk_seconds).ceil() as usize;

    let mut splits: Vec<f64> = Vec::new();
    let gap: f64 = seg_length / (num_segs as f64);

    for i in 1..num_segs {
        splits.push(seg.start + (gap * i as f64));
    }

    splits
}

/// Cut every segment down to `limit` seconds of audio, and throw away the ones
/// that start beyond it.
///
/// Used when "only process the first N minutes" is on. The words are clamped
/// too, not just the segment span: the splitter picks its cuts from word times,
/// so a word left pointing past the end is the thing that actually does the
/// damage.
fn clamp_segments_to(segments: &mut Vec<Segment>, limit: f64) {
    segments.retain(|s| s.start < limit);
    for seg in segments.iter_mut() {
        seg.end = seg.end.min(limit);
        seg.words.retain(|w| w.start < limit);
        for word in seg.words.iter_mut() {
            word.end = word.end.min(limit);
        }
    }
}

fn cut_times_to_chunks<'a>(
    samples: &'a [i16],
    seg: &Segment,
    cut_times: &[usize],
) -> Vec<Chunk<'a>> {
    let mut chunks = Vec::new();

    for pair in cut_times.windows(2) {
        // Clamp rather than index blindly. Everything upstream is meant to
        // hand over an ordered, in-bounds list, but "meant to" was doing the
        // work here: one inverted pair - start after end - panicked the whole
        // job thread, and a panicked worker shows up as a progress bar that
        // never moves rather than as an error anyone can act on. A bad cut
        // list should cost a chunk, not the job.
        let start = pair[0].min(samples.len());
        let end = pair[1].min(samples.len());
        if start >= end {
            continue;
        }

        chunks.push(Chunk {
            audio: &samples[start..end],
            text: seg.text.clone(),
            translation: seg.translation.clone().unwrap_or_default(),
        });
    }

    chunks
}

fn quietest_cut_time(samples: &[i16], gap_start_time: f64, gap_end_time: f64) -> f64 {
    const WINDOW_SECONDS: [f64; 14] = [
        1.00, 0.900, 0.800, 0.700, 0.600, 0.500, 0.400, 0.300, 0.200, 0.100, 0.070, 0.055, 0.040,
        0.020,
    ];

    const STEP_SECONDS: f64 = 0.005;

    let sample_rate = SAMPLE_RATE as f64;

    // Expand beyond Whisper's timestamps.
    let search_start_time = (gap_start_time).max(0.0);

    let search_end_time = gap_end_time;

    let mut search_start = (search_start_time * sample_rate) as usize;

    let mut search_end = (search_end_time * sample_rate) as usize;

    search_start = search_start.min(samples.len());
    search_end = search_end.min(samples.len());

    if search_end <= search_start {
        return (gap_start_time + gap_end_time) / 2.0;
    }

    let step_size = ((STEP_SECONDS * sample_rate) as usize).max(1);

    for window_seconds in WINDOW_SECONDS {
        let window_size = (window_seconds * sample_rate) as usize;

        let search_length = search_end - search_start;

        // Skip window sizes that cannot fit inside
        // the current search region.
        if window_size == 0 || window_size > search_length {
            continue;
        }

        let last_possible_start = search_end - window_size;

        let mut window_start = search_start;
        let mut best_window_start = search_start;
        let mut best_score = u64::MAX;

        while window_start <= last_possible_start {
            let window_end = window_start + window_size;

            let score: u64 = samples[window_start..window_end]
                .iter()
                .map(|&sample| (sample as i32).unsigned_abs() as u64)
                .sum();

            if score < best_score {
                best_score = score;
                best_window_start = window_start;
            }

            window_start += step_size;
        }

        // Search only within the quietest region
        // during the next, more precise stage.
        search_start = best_window_start;
        search_end = best_window_start + window_size;
    }

    let cut_sample = search_start + (search_end - search_start) / 2;

    cut_sample as f64 / sample_rate
}

/// Write an .ass subtitle file. White text, centered, large, on the
/// transparent video (which is black), timed to the output audio.
fn write_ass_subtitles(path: &Path, subs: &[SubtitleEntry]) -> Result<(), BoxErr> {
    let sr = SAMPLE_RATE as f64;
    let mut s = String::new();

    // Header. PlayResX/Y define the coordinate space; Alignment 5 = centered
    // both horizontally and vertically. Fontsize is in those play-res units.
    s.push_str(
        "[Script Info]\n\
         ScriptType: v4.00+\n\
         PlayResX: 1280\n\
         PlayResY: 720\n\
         ScaledBorderAndShadow: yes\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Default,Arial,54,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,5,40,40,40,1\n\n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );

    for sub in subs {
        let start = sub.start_sample as f64 / sr;
        let end = sub.end_sample as f64 / sr;
        // When there's a translation, show it at the same size, colour and
        // opacity as the original, on its own line with two blank lines between
        // them (three `\N` breaks) so the two languages read as separate blocks.
        let text = if sub.translation.trim().is_empty() {
            ass_escape(&sub.text)
        } else {
            format!(
                "{}\\N\\N\\N{}",
                ass_escape(&sub.text),
                ass_escape(&sub.translation),
            )
        };
        s.push_str(&format!(
            "Dialogue: 0,{},{},Default,,0,0,0,,{}\n",
            ass_timestamp(start),
            ass_timestamp(end),
            text,
        ));
    }

    std::fs::write(path, s)?;
    Ok(())
}

/// Format seconds as ASS timestamp H:MM:SS.cs (centiseconds).
fn ass_timestamp(seconds: f64) -> String {
    let total_cs = (seconds * 100.0).round() as i64;
    let cs = total_cs % 100;
    let total_s = total_cs / 100;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h}:{m:02}:{s:02}.{cs:02}")
}

/// Escape text for an ASS dialogue line. Newlines become \\N; braces would
/// be parsed as override tags so we strip them.
fn ass_escape(text: &str) -> String {
    text.replace('\n', "\\N")
        .replace('{', "(")
        .replace('}', ")")
}

/// A subtitle line: text shown from `start_sample` to `end_sample` in the
/// output audio timeline. `translation` is the English line shown beneath
/// `text` when the user enabled it, else empty.
struct SubtitleEntry {
    start_sample: usize,
    end_sample: usize,
    text: String,
    translation: String,
}

/// Build the repeated audio and, in lockstep, the subtitle timeline. Text
/// for a chunk stays on screen from the start of its first repeat through
/// the end of its final gap (per the chosen behavior).
fn repeat_chunks(
    chunks: &[Chunk],
    repeat_count: usize,
    gap_ratio: f64,
) -> (Vec<i16>, Vec<SubtitleEntry>) {
    if chunks.is_empty() || repeat_count == 0 {
        return (Vec::new(), Vec::new());
    }

    let gap_len_for = |chunk_len: usize| -> usize { (chunk_len as f64 * gap_ratio) as usize };

    let total: usize = chunks
        .iter()
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let is_last = chunk_index + 1 == chunks.len();
            let gaps_after = if is_last {
                repeat_count - 1
            } else {
                repeat_count
            };
            let gap_len = gap_len_for(chunk.audio.len());
            chunk.audio.len() * repeat_count + gap_len * gaps_after
        })
        .sum();

    let mut output: Vec<i16> = Vec::with_capacity(total);
    let mut subs: Vec<SubtitleEntry> = Vec::new();

    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let block_start = output.len();

        for repeat_index in 0..repeat_count {
            output.extend_from_slice(chunk.audio);

            let is_last_chunk = chunk_index + 1 == chunks.len();
            let is_last_repeat = repeat_index + 1 == repeat_count;

            if !(is_last_chunk && is_last_repeat) {
                let gap_length = gap_len_for(chunk.audio.len());
                output.resize(output.len() + gap_length, 0);
            }
        }

        let block_end = output.len();

        // Subtitle spans the whole block (all repeats + their gaps). Only
        // emit if there's text — split pieces and silent fragments have none.
        if !chunk.text.trim().is_empty() {
            subs.push(SubtitleEntry {
                start_sample: block_start,
                end_sample: block_end,
                text: chunk.text.clone(),
                translation: chunk.translation.clone(),
            });
        }
    }

    (output, subs)
}

/// True if this FFmpeg build includes the h264_nvenc encoder AND the
/// driver/hardware actually let us use it. Probed once and cached.
fn has_nvenc() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        // First check that the encoder is even compiled in. Most Windows
        // FFmpeg builds (gyan.dev, BtbN) ship with nvenc; on macOS via
        // Homebrew it's typically not present (no NVIDIA hardware).
        let list = no_window_command(crate::download::resolve_program("ffmpeg"))
            .args(["-hide_banner", "-encoders"])
            .output();
        let compiled_in = matches!(
            list,
            Ok(o) if String::from_utf8_lossy(&o.stdout).contains("h264_nvenc")
        );
        if !compiled_in {
            return false;
        }

        // Second, prove it actually works on this machine by encoding a
        // single frame to /dev/null. Catches the case where ffmpeg has
        // the encoder but no NVIDIA driver is present.
        let null_target = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let probe = no_window_command(crate::download::resolve_program("ffmpeg"))
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=1",
                "-vframes",
                "1",
                "-c:v",
                "h264_nvenc",
                "-f",
                "null",
                null_target,
            ])
            .output();
        matches!(probe, Ok(o) if o.status.success())
    })
}

fn encode_samples_to_mp4(
    samples: &[i16],
    output_path: &Path,
    subtitle_path: Option<&Path>,
    total_seconds: f64,
    cancel: &AtomicBool,
    on_frac: &dyn Fn(f32),
) -> Result<(), BoxErr> {
    use std::io::{BufRead, BufReader};

    let sample_rate_text = SAMPLE_RATE.to_string();

    // Write the PCM to a temp file so FFmpeg reads from a file and leaves
    // its stdout free for us to read -progress from. Avoids the stdin/stdout
    // pipe deadlock entirely.
    let raw_path = unique_work_dir("pcm").with_extension("raw");
    if let Some(parent) = raw_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    {
        let f = std::fs::File::create(&raw_path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, f);
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                samples.as_ptr() as *const u8,
                std::mem::size_of_val(samples),
            )
        };
        writer.write_all(bytes)?;
        writer.flush()?;
    }

    let use_nvenc = has_nvenc();
    let video_codec_args: &[&str] = if use_nvenc {
        &["-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ll"]
    } else {
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "stillimage",
        ]
    };

    let subtitle_filter: Option<String> = subtitle_path.map(|p| {
        let raw = p.to_string_lossy().replace('\\', "/");
        let escaped = raw.replace(':', "\\:");
        format!("subtitles='{escaped}'")
    });

    let mut cmd = no_window_command(crate::download::resolve_program("ffmpeg"));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "s16le", "-ar", &sample_rate_text, "-ac", "1"])
        .arg("-i")
        .arg(&raw_path)
        .args([
            "-f",
            "lavfi",
            // Low framerate + low resolution: static text on black has no
            // motion and doesn't need HD. 854x480 is ~2.5x fewer pixels than
            // 720p for libass to composite and NVENC to encode, with text
            // still perfectly readable.
            "-i",
            "color=c=black:s=854x480:r=2",
            "-shortest",
        ]);
    if let Some(ref filter) = subtitle_filter {
        cmd.args(["-vf", filter]);
    }
    cmd.args(video_codec_args)
        .args(["-pix_fmt", "yuv420p"])
        // Always MP4 + AAC: broadly compatible, plays everywhere.
        .args(["-c:a", "aac", "-b:a", "192k"])
        // Emit machine-readable progress to stdout.
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(output_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    // Read progress lines as they stream. FFmpeg writes key=value lines;
    // we care about out_time_us (microseconds encoded so far).
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        let total_us = (total_seconds * 1_000_000.0).max(1.0);
        for line in reader.lines().map_while(Result::ok) {
            // FFmpeg emits a progress block regularly, so a between-lines check
            // cancels an encode within a fraction of a second. Killing it ends
            // the stream and wait_with_output below returns.
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            if let Some(val) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = val.trim().parse::<f64>() {
                    let frac = (us / total_us).clamp(0.0, 1.0) as f32;
                    on_frac(frac);
                }
            }
        }
    }

    let output = child.wait_with_output()?;

    // Clean up the temp PCM file.
    let _ = std::fs::remove_file(&raw_path);

    if cancel.load(Ordering::Relaxed) {
        return Err("encoding cancelled".into());
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "FFmpeg failed while encoding:\n{}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        )
        .into());
    }

    on_frac(1.0);
    Ok(())
}
