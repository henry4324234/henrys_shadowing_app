#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

mod deps;
mod download;
mod pipeline;
mod theme;

use pipeline::{fetch_video_title, run_pipeline, CookieSource, Source, Stage, WhisperModel};

/// Global UI zoom, on top of the OS DPI scaling egui already applies. Scales
/// text, controls, and glyph icons together, and the window is sized up by the
/// same factor so nothing gets cramped. Bump this to make everything bigger.
const UI_SCALE: f32 = 1.3;

/// How many jobs run at once by default, before the user changes it. The live
/// value is `App::max_concurrent_jobs`, which the scheduler reads every frame.
const DEFAULT_MAX_CONCURRENT_JOBS: usize = 2;
/// Upper bound the concurrency picker offers. Two is plenty for a GPU pipeline;
/// this leaves headroom without inviting someone to launch twenty at once.
const MAX_CONCURRENT_LIMIT: usize = 8;
const BEEP_DEBOUNCE_MS: u128 = 500;
const DEFAULT_REPEAT_COUNT: usize = 3;
const DEFAULT_GAP_RATIO: f64 = 1.5;
const DEFAULT_MAX_CHUNK_SECONDS: f64 = 8.0;
const DEFAULT_MAX_DURATION_MINUTES: f64 = 5.0;
const MAX_URL_HISTORY: usize = 20;

#[derive(Clone, Debug)]
enum JobStatus {
    Pending,
    Queued,
    Running(Stage),
    Done(PathBuf),
    Failed(String),
}

impl JobStatus {
    fn is_active(&self) -> bool {
        matches!(self, JobStatus::Queued | JobStatus::Running(_))
    }

    fn is_finished(&self) -> bool {
        matches!(self, JobStatus::Done(_) | JobStatus::Failed(_))
    }
}

/// One remembered URL, plus its video title once yt-dlp has told us what it is.
/// `title` stays None while the lookup is in flight, and forever if it failed —
/// the URL is always shown as the fallback.
#[derive(Clone)]
struct UrlEntry {
    url: String,
    title: Option<String>,
}

struct Job {
    id: u64,
    source: Source,
    status: JobStatus,
    /// The video's title, for URL jobs, once fetched in the background. None
    /// for file jobs (the filename is the name) and until the lookup lands.
    title: Option<String>,
    /// When the current Running(stage) began — used to estimate progress.
    stage_started: std::time::Instant,
    /// Real progress fraction (0..1) if the current stage reports one;
    /// None means use the time-based estimate instead.
    real_progress: Option<f32>,
    /// The pipeline stages this job will run through, in order, captured
    /// when it's queued (that's when its settings are locked in). Empty until
    /// then. Drives the little stage dots next to the job's name.
    planned_stages: Vec<Stage>,
    /// The settings snapshot this job runs with, taken when it leaves Pending
    /// (so a whole "Start all" batch shares one set of settings even if the
    /// user changes them while jobs are queued). None until queued.
    config: Option<JobConfig>,
    /// Set when the job has a live worker thread (running, or being cancelled).
    /// The scheduler flips the flag to true to stop the worker, and won't
    /// re-run the job until the worker clears it by exiting. None when no
    /// worker is attached.
    cancel: Option<Arc<AtomicBool>>,
    /// Bumped each time a worker is spawned for this job. Worker messages carry
    /// the generation they were sent under; ones from a superseded worker (a
    /// mismatched generation) are ignored.
    generation: u64,
    /// Assigned when the job is promoted to Running — a monotonic "started at"
    /// order, so the scheduler can cancel the *most recently* started jobs first
    /// when the concurrency limit is lowered.
    start_seq: u64,
}

/// A snapshot of the settings a job runs with, captured when it's queued so the
/// worker isn't reading live `App` state from another thread (and so a queued
/// job keeps the settings it was launched with rather than picking up later
/// edits). Everything `run_pipeline` needs, minus the source and cancel token.
#[derive(Clone)]
struct JobConfig {
    whisper_model: WhisperModel,
    strip_music: bool,
    cookie_source: CookieSource,
    repeat_count: usize,
    gap_ratio: f64,
    max_chunk_seconds: f64,
    max_duration_seconds: Option<f64>,
    show_text: bool,
    translate_english: bool,
    output_dir: PathBuf,
}

impl Job {
    fn display_name(&self) -> String {
        match &self.source {
            Source::File(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            // Show the video's title once we have it; the raw URL until then.
            Source::Url(url) => match &self.title {
                Some(title) => ellipsize(title, 60),
                None => ellipsize(url, 60),
            },
        }
    }
}

/// Whether a fetched title is worth keeping.
///
/// U+FFFD is what `from_utf8_lossy` leaves behind when bytes couldn't be
/// decoded, and no font has a glyph for it — so such a title renders as a row
/// of empty boxes. Titles cached before yt-dlp was told to emit UTF-8 (see
/// `YT_DLP_UTF8`) look exactly like this, and `add_url` would otherwise reuse
/// them forever. Rejecting them makes those entries re-fetch and self-heal.
fn title_is_usable(title: &str) -> bool {
    !title.contains('\u{FFFD}')
}

/// Truncate to `max_chars` with an ellipsis. Counts characters, not bytes —
/// video titles are routinely non-ASCII and byte-slicing them panics.
fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let kept: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Like `ellipsize` but keeps the *end* of the string — the informative part
/// of a filesystem path ("…\Videos\Downloads"), not the drive letter.
fn ellipsize_start(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let kept: String = s
        .chars()
        .skip(count - max_chars.saturating_sub(1))
        .collect();
    format!("…{kept}")
}

/// True if every character of `text` has a real glyph in the body font's
/// fallback chain. Video titles in scripts our bundled fonts don't cover
/// (CJK, Thai, …) would render as rows of empty boxes; callers show the URL
/// instead when this fails.
fn renders_cleanly(ui: &egui::Ui, text: &str) -> bool {
    let font = egui::TextStyle::Body.resolve(ui.style());
    ui.fonts(|f| f.has_glyphs(&font, text))
}

/// With the OS title bar turned off, Windows also stops decorating the
/// window — no rounded corners, no thin border, no drop shadow. These DWM
/// calls ask for each of them back without bringing back the bar itself.
#[cfg(target_os = "windows")]
mod win_chrome {
    use std::sync::atomic::{AtomicIsize, Ordering};

    /// The main window's handle, stashed at startup so the border can be
    /// re-tinted whenever the color scheme changes.
    static HWND: AtomicIsize = AtomicIsize::new(0);

    /// dwmapi's MARGINS struct (cxLeftWidth, cxRightWidth, cyTopHeight,
    /// cyBottomHeight).
    #[repr(C)]
    struct Margins {
        left: i32,
        right: i32,
        top: i32,
        bottom: i32,
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            value: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
        fn DwmExtendFrameIntoClientArea(hwnd: isize, margins: *const Margins) -> i32;
    }

    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_ROUND: i32 = 2;
    const DWMWA_BORDER_COLOR: u32 = 34;

    /// One-time setup: rounded corners and the standard drop shadow.
    /// Best-effort — on Windows 10 the corner/border attributes don't exist
    /// and the calls just return an error that we ignore.
    pub fn init(cc: &eframe::CreationContext<'_>, border: eframe::egui::Color32) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => return,
        };
        HWND.store(hwnd, Ordering::Relaxed);

        unsafe {
            // The same corner radius every decorated window gets on Win 11.
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &round as *const _ as *const _,
                std::mem::size_of::<i32>() as u32,
            );

            // Extending the invisible DWM frame one pixel into the (fully
            // opaque, so nothing shows) client area is the long-standing
            // trick that makes DWM draw its standard drop shadow around a
            // frameless window.
            let margins = Margins {
                left: 0,
                right: 0,
                top: 1,
                bottom: 0,
            };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        }

        set_border_color(border);
    }

    /// Tint the thin 1px window outline to match the current scheme.
    pub fn set_border_color(color: eframe::egui::Color32) {
        let hwnd = HWND.load(Ordering::Relaxed);
        if hwnd == 0 {
            return;
        }
        // COLORREF byte order is 0x00BBGGRR.
        let colorref: u32 =
            (color.r() as u32) | ((color.g() as u32) << 8) | ((color.b() as u32) << 16);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &colorref as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

/// How close to a window edge (in points) the pointer counts as a resize
/// grip. The OS resize borders went away with the title bar, so the app
/// provides its own.
const RESIZE_GRIP_MARGIN: f32 = 6.0;

/// Which way dragging from the pointer's current position would resize the
/// window: Some(edge/corner) when the pointer sits within the grip margin,
/// None in the interior (or when maximized, where resizing means nothing).
fn resize_direction_at_pointer(ctx: &egui::Context) -> Option<egui::viewport::ResizeDirection> {
    use egui::viewport::ResizeDirection;

    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return None;
    }
    let pos = ctx.input(|i| i.pointer.hover_pos())?;
    let rect = ctx.screen_rect();
    let m = RESIZE_GRIP_MARGIN;
    let left = pos.x - rect.left() <= m;
    let right = rect.right() - pos.x <= m;
    let top = pos.y - rect.top() <= m;
    let bottom = rect.bottom() - pos.y <= m;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => ResizeDirection::NorthWest,
        (_, true, true, _) => ResizeDirection::NorthEast,
        (true, _, _, true) => ResizeDirection::SouthWest,
        (_, true, _, true) => ResizeDirection::SouthEast,
        (true, _, _, _) => ResizeDirection::West,
        (_, true, _, _) => ResizeDirection::East,
        (_, _, true, _) => ResizeDirection::North,
        (_, _, _, true) => ResizeDirection::South,
        _ => return None,
    })
}

/// Which window control a title-bar button stands for.
#[derive(Clone, Copy, PartialEq)]
enum WinButton {
    Minimize,
    Maximize,
    Close,
}

/// One minimize/maximize/close button for the custom title bar. The icons are
/// painted with line primitives rather than font glyphs, so they can't fall
/// victim to missing-glyph boxes. Returns true when clicked.
fn titlebar_button(ui: &mut egui::Ui, kind: WinButton) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(36.0, 24.0), egui::Sense::click());
    let p = theme::palette();

    if response.hovered() {
        // Close hovers red, like every Windows app the user knows.
        let bg = if kind == WinButton::Close {
            p.error
        } else {
            p.widget_hover
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(6), bg);
    }

    let color = if response.hovered() {
        if kind == WinButton::Close {
            egui::Color32::WHITE
        } else {
            p.text_strong
        }
    } else {
        p.text_muted
    };
    let stroke = egui::Stroke::new(1.4, color);
    let c = rect.center();
    let r = 4.5;
    let painter = ui.painter();
    match kind {
        WinButton::Minimize => {
            painter.line_segment([c + egui::vec2(-r, 0.0), c + egui::vec2(r, 0.0)], stroke);
        }
        WinButton::Maximize => {
            painter.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(2.0 * r, 2.0 * r)),
                egui::CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        WinButton::Close => {
            painter.line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
            painter.line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], stroke);
        }
    }

    response.clicked()
}

/// Open the system file manager with `path` highlighted (or at least its
/// folder showing). Best-effort — failures are silently ignored.
fn reveal_in_folder(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(dir) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let _ = path;
}

enum JobMsg {
    // The u64 after the job id is the worker *generation* (see `Job::generation`)
    // — messages from a superseded worker are dropped by comparing it.
    Stage(u64, u64, Stage, Option<f32>),
    Done(u64, u64, PathBuf),
    Failed(u64, u64, String),
    /// The worker stopped because the job was cancelled (concurrency lowered).
    /// The job is left Queued to run again when a slot frees.
    Cancelled(u64, u64),
    /// The background title lookup for a URL job came back.
    Title(u64, String),
}

/// Decrements the live-worker counter when a worker thread ends, however it
/// ends (normal return, early error, or panic). The counter is incremented on
/// the UI thread as the worker is spawned, so the scheduler never sees a stale
/// over-count that would let it launch past the concurrency limit.
struct WorkerGuard(Arc<AtomicUsize>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Phase of the in-app tool download (see download.rs). Kept separate from
/// the byte counts because verifying/extracting have no meaningful fraction.
#[derive(Clone, Copy)]
enum DlPhase {
    Downloading,
    Verifying,
    Extracting,
}

/// UI-side state of the single tool download that can run at a time.
struct ActiveDownload {
    id: download::ToolId,
    phase: DlPhase,
    done: u64,
    total: Option<u64>,
}

/// Rough time constant for each stage, in seconds, feeding the ease-out curve
/// in `estimated_progress`. Deliberately approximate — the curve eases toward
/// 95% and keeps creeping, so an under-estimate just means the bar advances a
/// little slower than real and an over-estimate a little faster, never a freeze.
/// Tuned for a GPU machine; CPU is slower but the curve keeps moving regardless.
fn stage_estimate_seconds(stage: &Stage) -> f32 {
    match stage {
        Stage::Downloading => 15.0,
        Stage::Separating => 30.0,
        Stage::Transcribing => 30.0,
        // Translation is a second full engine pass, so budget it like the first.
        Stage::Translating => 30.0,
        Stage::Decoding => 5.0,
        Stage::Splitting => 2.0,
        Stage::Encoding => 15.0,
    }
}

/// The stages a job will actually run, in the order run_pipeline runs them.
/// Must mirror run_pipeline: Downloading (URLs only) → Separating (if
/// stripping music) → Transcribing → Translating (if requested) → Decoding →
/// Splitting → Encoding.
fn planned_stages(source: &Source, strip_music: bool, translate_english: bool) -> Vec<Stage> {
    let mut stages = Vec::new();
    if matches!(source, Source::Url(_)) {
        stages.push(Stage::Downloading);
    }
    if strip_music {
        stages.push(Stage::Separating);
    }
    stages.push(Stage::Transcribing);
    if translate_english {
        stages.push(Stage::Translating);
    }
    stages.push(Stage::Decoding);
    stages.push(Stage::Splitting);
    stages.push(Stage::Encoding);
    stages
}

/// A row of small dots, one per planned pipeline stage: green = finished,
/// amber = happening now, faint = still to come. Hover a dot for its name.
fn stage_dots(ui: &mut egui::Ui, stages: &[Stage], status: &JobStatus) {
    // Index of the stage currently running; len() means all of them are done,
    // None means none has started yet (queued).
    let current: Option<usize> = match status {
        JobStatus::Queued => None,
        JobStatus::Running(stage) => stages.iter().position(|s| s == stage),
        JobStatus::Done(_) => Some(stages.len()),
        _ => return,
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        for (i, stage) in stages.iter().enumerate() {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            let (color, state) = match current {
                Some(c) if i < c => (theme::palette().success, "finished"),
                Some(c) if i == c => (theme::palette().accent, "in progress"),
                _ => (theme::palette().widget_hover, "waiting"),
            };
            ui.painter().circle_filled(rect.center(), 3.5, color);
            response.on_hover_text(format!("{} — {state}", stage.label()));
        }
    });
}

/// Time-based progress estimate for stages that can't report a real fraction.
///
/// Eases out toward — but never reaches — 95%: `1 - e^(-t/τ)` climbs quickly at
/// first and then ever more slowly, so the bar is always visibly creeping
/// forward even when a stage overruns its estimate. That's the whole point:
/// the old hard cap hit 95% and sat frozen, which read as "hung" (badly so for
/// the long second pass when translating). This keeps moving instead.
fn estimated_progress(elapsed: f32, estimate: f32) -> f32 {
    let tau = estimate.max(0.1);
    0.95 * (1.0 - (-elapsed / tau).exp())
}

/// Overall completion of a job (0..1), across *all* its planned stages, given
/// how far the current stage has got.
///
/// The old bar showed only the current stage's fraction, so it snapped back to
/// zero at every stage boundary — a job three stages deep still read "0%" the
/// instant a new stage began. Instead, weight each stage by its rough expected
/// duration (`stage_estimate_seconds`): completed stages contribute their whole
/// weight, the current one contributes its weight times its own fraction, and
/// the total is normalised to the sum of all weights. That makes the bar a real,
/// monotonic measure of how much of the whole job is done — the stage name and
/// the dots still say *which* step is running.
fn overall_progress(planned: &[Stage], current: &Stage, stage_frac: f32) -> f32 {
    if planned.is_empty() {
        return stage_frac.clamp(0.0, 1.0);
    }
    let total: f32 = planned.iter().map(stage_estimate_seconds).sum();
    if total <= 0.0 {
        return stage_frac.clamp(0.0, 1.0);
    }
    // Index of the current stage; if it isn't in the plan (shouldn't happen),
    // treat everything before as unknown and just show the stage fraction.
    let Some(idx) = planned.iter().position(|s| s == current) else {
        return stage_frac.clamp(0.0, 1.0);
    };
    let before: f32 = planned[..idx].iter().map(stage_estimate_seconds).sum();
    let current_weight = stage_estimate_seconds(current);
    ((before + current_weight * stage_frac.clamp(0.0, 1.0)) / total).clamp(0.0, 1.0)
}

struct App {
    jobs: Vec<Job>,
    next_id: u64,
    whisper_model: WhisperModel,
    strip_music: bool,
    repeat_count: usize,
    gap_ratio: f64,
    max_chunk_seconds: f64,
    limit_duration: bool,
    max_duration_minutes: f64,
    show_text: bool,
    translate_english: bool,
    /// Global: when on, every job beeps as it finishes or fails; when off, none
    /// do. Read live at finish time, so toggling it affects jobs already running.
    beep_on_finish: bool,
    /// Global: how many jobs the scheduler runs at once. Lowering it below the
    /// number currently running cancels the most-recently-started ones and puts
    /// them back in the queue (see `schedule_jobs`).
    max_concurrent_jobs: usize,
    /// Index into `theme::SCHEMES` — which color scheme the UI wears.
    color_scheme: usize,
    last_beep: Option<std::time::Instant>,
    cookie_source: CookieSource,
    output_dir: PathBuf,
    url_input: String,
    url_history: Vec<UrlEntry>,
    /// True while the transcription-engine window is showing.
    whisper_window_open: bool,
    /// Cached "Whisper is present" so we don't re-probe every frame, or on
    /// every Start-all after it first passes. Set at startup, and refreshed
    /// after a successful engine download.
    whisper_ok: bool,
    /// True while the Demucs / "Strip music" explainer window is showing.
    demucs_window_open: bool,
    rx: Receiver<JobMsg>,
    tx: Sender<JobMsg>,
    /// Number of worker threads currently alive. Incremented on the UI thread
    /// as a worker is spawned, decremented by the worker's `WorkerGuard` when it
    /// ends. The scheduler reads it to decide how many free slots there are.
    worker_count: Arc<AtomicUsize>,
    /// Next value for `Job::start_seq`.
    next_start_seq: u64,
    /// Next value for `Job::generation`.
    next_generation: u64,
    dl_tx: Sender<download::DownloadMsg>,
    dl_rx: Receiver<download::DownloadMsg>,
    dl_active: Option<ActiveDownload>,
    dl_error: Option<String>,
    dl_cancel: Arc<AtomicBool>,
}

impl Default for App {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();

        let (dl_tx, dl_rx) = crossbeam_channel::unbounded();

        let output_dir = default_output_dir();

        let mut app = Self {
            jobs: Vec::new(),
            next_id: 0,
            whisper_model: WhisperModel::Small,
            // Off until the user turns it on and the Demucs check passes, so
            // it's never enabled without the tool behind it. Their choice is
            // then persisted, so this default only applies on a first run.
            strip_music: false,
            repeat_count: DEFAULT_REPEAT_COUNT,
            gap_ratio: DEFAULT_GAP_RATIO,
            max_chunk_seconds: DEFAULT_MAX_CHUNK_SECONDS,
            limit_duration: false,
            max_duration_minutes: DEFAULT_MAX_DURATION_MINUTES,
            show_text: false,
            translate_english: false,
            beep_on_finish: true,
            max_concurrent_jobs: DEFAULT_MAX_CONCURRENT_JOBS,
            color_scheme: 0,
            last_beep: None,
            cookie_source: default_cookie_source(),
            output_dir,
            url_input: String::new(),
            url_history: Vec::new(),
            whisper_window_open: false,
            whisper_ok: false,
            demucs_window_open: false,
            rx,
            tx,
            worker_count: Arc::new(AtomicUsize::new(0)),
            next_start_seq: 0,
            next_generation: 0,
            dl_tx,
            dl_rx,
            dl_active: None,
            dl_error: None,
            dl_cancel: Arc::new(AtomicBool::new(false)),
        };

        // Overlay any persisted settings (best-effort — failures silently
        // fall back to defaults).
        if let Some(saved) = load_settings() {
            saved.apply(&mut app);
        }

        // Deliberately no dependency check here: probing spawns subprocesses and
        // would stall the window opening. The check runs the first time "Start
        // all" is pressed instead, and its result is cached from then on.
        app
    }
}

impl App {
    fn add_file_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            self.jobs.push(Job {
                id: self.next_id,
                source: Source::File(path),
                status: JobStatus::Pending,
                title: None,
                stage_started: std::time::Instant::now(),
                real_progress: None,
                planned_stages: Vec::new(),
                config: None,
                cancel: None,
                generation: 0,
                start_seq: 0,
            });
            self.next_id += 1;
        }
    }

    fn add_url(&mut self, url: String) {
        // Track this URL in recent history (most recent first, deduped, capped).
        // Re-adding a URL we already know the title of keeps that title rather
        // than re-fetching it from scratch.
        let known_title = self
            .url_history
            .iter()
            .find(|e| e.url == url)
            .and_then(|e| e.title.clone())
            .filter(|t| title_is_usable(t));
        self.url_history.retain(|e| e.url != url);
        self.url_history.insert(
            0,
            UrlEntry {
                url: url.clone(),
                title: known_title.clone(),
            },
        );
        self.url_history.truncate(MAX_URL_HISTORY);

        let id = self.next_id;
        self.jobs.push(Job {
            id,
            source: Source::Url(url.clone()),
            status: JobStatus::Pending,
            title: known_title.clone(),
            stage_started: std::time::Instant::now(),
            real_progress: None,
            planned_stages: Vec::new(),
            config: None,
            cancel: None,
            generation: 0,
            start_seq: 0,
        });
        self.next_id += 1;

        // Look the title up in the background so the queue shows the video's
        // name instead of a raw URL. Best-effort: if it fails the URL stays as
        // the label, and the job itself is unaffected either way.
        if known_title.is_none() {
            let tx = self.tx.clone();
            let cookie_source = self.cookie_source.clone();
            thread::spawn(move || {
                if let Some(title) = fetch_video_title(&url, cookie_source) {
                    let _ = tx.send(JobMsg::Title(id, title));
                }
            });
        }
    }

    fn submit_url_input(&mut self) {
        let url = self.url_input.trim().to_string();
        if !url.is_empty() {
            self.add_url(url);
            self.url_input.clear();
        }
    }

    fn maybe_beep(&mut self, success: bool) {
        if !self.beep_on_finish {
            return;
        }
        // Debounce so a batch finishing in a clump doesn't make an
        // overlapping racket.
        let now = std::time::Instant::now();
        if let Some(last) = self.last_beep {
            if now.duration_since(last).as_millis() < BEEP_DEBOUNCE_MS {
                return;
            }
        }
        self.last_beep = Some(now);
        play_beep(success);
    }

    /// Kick off a managed download of one tool. Single-flight: ignored if
    /// another download is already running (the buttons are disabled then
    /// anyway, but drag-lag can slip a second click through).
    fn start_tool_download(&mut self, id: download::ToolId) {
        if self.dl_active.is_some() {
            return;
        }
        self.dl_error = None;
        self.dl_cancel = Arc::new(AtomicBool::new(false));
        self.dl_active = Some(ActiveDownload {
            id,
            phase: DlPhase::Downloading,
            done: 0,
            total: Some(download::spec(id).approx_size),
        });
        let _ = download::spawn_install(id, self.dl_tx.clone(), self.dl_cancel.clone());
    }

    /// The transcription-engine window: required, so it opens itself at startup
    /// and on Start-all when the engine is missing. Explains why it's needed and
    /// offers the one-click download.
    fn whisper_window(&mut self, ctx: &egui::Context) {
        if !self.whisper_window_open {
            return;
        }
        let mut open = true;
        let mut close = false;
        let mut download = false;
        let installed = self.whisper_ok;
        let download_busy = self.dl_active.is_some();

        egui::Window::new("Transcription engine")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    "This app works by transcribing your audio into sentences, then \
                     cutting and repeating each one. That transcription is done by \
                     Whisper — without it there's nothing to split, so it's required \
                     before any clip can be made.",
                );
                ui.add_space(8.0);

                if installed {
                    ui.colored_label(theme::palette().success, "✔ Installed and ready.");
                } else {
                    ui.label(
                        "It isn't installed yet. The one-click download below fetches \
                         the standalone engine — a pinned, checksum-verified copy just \
                         for this app. It needs no Python and isn't added to your \
                         system. It's a large file (~1.4 GB), so it takes a while.",
                    );
                    ui.add_space(6.0);
                    if let Some(id) = download_button(ui, download::ToolId::FasterWhisper, download_busy)
                    {
                        let _ = id; // always FasterWhisper here
                        download = true;
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(deps::WHISPER_INSTALL_HINT)
                            .monospace()
                            .small()
                            .color(theme::palette().text_muted),
                    );
                }

                self.ui_download_status(ui);

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
            });

        if download {
            self.start_tool_download(download::ToolId::FasterWhisper);
        }
        // The [x] titlebar button and Close both dismiss it. Closing while the
        // engine is still missing is allowed — Start all will re-open it.
        self.whisper_window_open = open && !close;
    }

    /// The Demucs / "Strip music" explainer. Opens when the user ticks Strip
    /// music without Demucs installed. Demucs is a Python package with no
    /// managed download, so this is install guidance rather than a button.
    fn demucs_window(&mut self, ctx: &egui::Context) {
        if !self.demucs_window_open {
            return;
        }
        let mut open = true;
        let mut close = false;
        let mut recheck = false;

        egui::Window::new("About “Strip music”")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("What it does").strong());
                ui.label(
                    "“Strip music” runs Demucs, an open-source AI model that separates \
                     a track into vocals and instruments. It isolates the speech first \
                     so background music doesn't confuse the transcription.",
                );
                ui.add_space(8.0);

                ui.label(egui::RichText::new("When you need it").strong());
                ui.label(
                    "Only for audio with loud backing music — songs, some videos. For \
                     plain speech leave it off; everything else works without Demucs, \
                     and it's slow on a CPU.",
                );
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Installing it").strong());
                ui.label(
                    egui::RichText::new(deps::DEMUCS_INSTALL_HINT)
                        .monospace()
                        .small()
                        .color(theme::palette().text_muted),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("I've installed it")
                        .on_hover_text("Re-check for Demucs and turn Strip music on")
                        .clicked()
                    {
                        recheck = true;
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if recheck && deps::check_demucs().is_ok() {
            self.strip_music = true;
            close = true;
        }
        self.demucs_window_open = open && !close;
    }

    /// Shared active-download UI (progress bar / verifying / extracting spinner,
    /// and the last error). Reads download state; the cancel button flips the
    /// shared atomic, which the worker checks between reads.
    fn ui_download_status(&self, ui: &mut egui::Ui) {
        if let Some(active) = &self.dl_active {
            ui.add_space(6.0);
            let spec = download::spec(active.id);
            match active.phase {
                DlPhase::Downloading => {
                    let frac = match active.total {
                        Some(t) if t > 0 => (active.done as f32 / t as f32).min(1.0),
                        _ => 0.0,
                    };
                    ui.label(
                        egui::RichText::new(format!("Downloading {}", spec.display_name))
                            .small()
                            .color(theme::palette().text_muted),
                    );
                    theme::progress_bar(
                        ui,
                        frac,
                        &download::progress_label(active.done, active.total),
                    );
                    if theme::ghost_button(ui, "Cancel download").clicked() {
                        self.dl_cancel
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                DlPhase::Verifying => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(theme::palette().accent));
                        ui.label(format!("Verifying {}…", spec.display_name));
                    });
                }
                DlPhase::Extracting => match active.total {
                    // A real bar: this step takes minutes on the 4.5 GB engine,
                    // and a bare spinner for that long reads as a hang.
                    Some(total) if total > 0 => {
                        let frac = (active.done as f32 / total as f32).min(1.0);
                        ui.label(
                            egui::RichText::new(format!("Extracting {}", spec.display_name))
                                .small()
                                .color(theme::palette().text_muted),
                        );
                        theme::progress_bar(
                            ui,
                            frac,
                            &download::progress_label(active.done, Some(total)),
                        );
                        if theme::ghost_button(ui, "Cancel").clicked() {
                            self.dl_cancel
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0).color(theme::palette().accent));
                            ui.label(format!("Extracting {}…", spec.display_name));
                        });
                    }
                },
            }
        }

        if let Some(err) = &self.dl_error {
            ui.add_space(4.0);
            ui.colored_label(theme::palette().error, format!("Download failed: {err}"));
        }
    }

    /// Snapshot the current settings as the config a job will run with.
    fn current_job_config(&self) -> JobConfig {
        JobConfig {
            whisper_model: self.whisper_model,
            strip_music: self.strip_music,
            cookie_source: self.cookie_source.clone(),
            repeat_count: self.repeat_count,
            gap_ratio: self.gap_ratio,
            max_chunk_seconds: self.max_chunk_seconds,
            max_duration_seconds: if self.limit_duration {
                Some(self.max_duration_minutes * 60.0)
            } else {
                None
            },
            show_text: self.show_text,
            translate_english: self.translate_english,
            output_dir: self.output_dir.clone(),
        }
    }

    /// Queue every Pending job with the current settings. Actually running them
    /// is the scheduler's job (`schedule_jobs`), which honours the concurrency
    /// limit; this only moves jobs from Pending to Queued and locks in their
    /// settings so the whole batch shares one snapshot.
    fn start_pending(&mut self) {
        // Persist current settings before starting work; if the user quits
        // mid-job their tuning still sticks for next time.
        save_settings(self);

        let config = self.current_job_config();
        for job in self
            .jobs
            .iter_mut()
            .filter(|j| matches!(j.status, JobStatus::Pending))
        {
            job.planned_stages =
                planned_stages(&job.source, config.strip_music, config.translate_english);
            job.config = Some(config.clone());
            job.status = JobStatus::Queued;
        }
    }

    /// Bring the running set into line with `max_concurrent_jobs`: cancel the
    /// most-recently-started jobs when we're over the limit, then fill any free
    /// slots with the oldest waiting jobs. Runs every frame while work is
    /// active, so raising or lowering the limit takes effect promptly.
    fn schedule_jobs(&mut self) {
        let max = self.max_concurrent_jobs.max(1);

        // Over the limit: cancel the newest running jobs until we're back at it.
        // A cancelled job goes straight back to Queued; its worker keeps the
        // cancel flag set and shuts down in the background, and won't be re-run
        // until it clears the flag by sending Cancelled.
        loop {
            let running: Vec<usize> = self
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| matches!(j.status, JobStatus::Running(_)))
                .map(|(i, _)| i)
                .collect();
            if running.len() <= max {
                break;
            }
            // Newest first = highest start_seq.
            let Some(victim) = running
                .into_iter()
                .max_by_key(|&i| self.jobs[i].start_seq)
            else {
                break;
            };
            let job = &mut self.jobs[victim];
            if let Some(token) = &job.cancel {
                token.store(true, Ordering::SeqCst);
            }
            job.status = JobStatus::Queued;
            job.real_progress = None;
        }

        // Free slots: start the oldest eligible waiting jobs. Eligible = Queued
        // with no live worker still attached (a just-cancelled job stays Queued
        // but keeps its dying worker until it exits, and mustn't be double-run).
        let live = self.worker_count.load(Ordering::SeqCst);
        let slots = max.saturating_sub(live);
        if slots > 0 {
            let ready: Vec<usize> = self
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| matches!(j.status, JobStatus::Queued) && j.cancel.is_none())
                .map(|(i, _)| i)
                .take(slots)
                .collect();
            for idx in ready {
                self.spawn_worker(idx);
            }
        }
    }

    /// Promote the job at `idx` to Running and launch its worker thread.
    fn spawn_worker(&mut self, idx: usize) {
        let generation = self.next_generation;
        self.next_generation += 1;
        let start_seq = self.next_start_seq;
        self.next_start_seq += 1;

        let token = Arc::new(AtomicBool::new(false));

        let (id, source, config) = {
            let job = &mut self.jobs[idx];
            let Some(config) = job.config.clone() else {
                // Shouldn't happen — only queued jobs are scheduled, and queuing
                // sets the config. Guard rather than panic.
                return;
            };
            job.generation = generation;
            job.start_seq = start_seq;
            job.cancel = Some(token.clone());
            job.stage_started = std::time::Instant::now();
            job.real_progress = None;
            // Show Running immediately at its first stage; the worker's first
            // progress message confirms it a moment later.
            let first = job
                .planned_stages
                .first()
                .copied()
                .unwrap_or(Stage::Transcribing);
            job.status = JobStatus::Running(first);
            (job.id, job.source.clone(), config)
        };

        // Count the worker before spawning it, on this thread, so the scheduler
        // can't over-promote in the gap before the thread starts running.
        self.worker_count.fetch_add(1, Ordering::SeqCst);
        let worker_count = self.worker_count.clone();
        let tx = self.tx.clone();

        thread::spawn(move || {
            let _guard = WorkerGuard(worker_count);

            let result = run_pipeline(
                &source,
                &config.output_dir,
                config.whisper_model,
                config.strip_music,
                config.cookie_source,
                config.repeat_count,
                config.gap_ratio,
                config.max_chunk_seconds,
                config.max_duration_seconds,
                config.show_text,
                config.translate_english,
                &token,
                &|stage, frac| {
                    let _ = tx.send(JobMsg::Stage(id, generation, stage, frac));
                },
            );

            // A completed run is always kept, even if the cancel flag was set in
            // the instant after it finished — the output exists, so honour it.
            // Only a run that *errored* while cancelled is a cancellation (the
            // job returns to the queue); an error with the flag clear is a real
            // failure.
            let msg = match result {
                Ok(path) => JobMsg::Done(id, generation, path),
                Err(e) => {
                    if token.load(Ordering::SeqCst) {
                        JobMsg::Cancelled(id, generation)
                    } else {
                        JobMsg::Failed(id, generation, e.to_string())
                    }
                }
            };
            let _ = tx.send(msg);
        });
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        save_settings(self);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                JobMsg::Stage(id, generation, stage, frac) => {
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        // Drop updates from a superseded worker, and from the
                        // current one once it's been told to cancel (its status
                        // is already Queued and must stay there).
                        let stale = j.generation != generation;
                        let cancelling =
                            j.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
                        if !stale && !cancelling {
                            // Reset the timer only when the stage actually changes,
                            // so progress within a stage keeps advancing smoothly.
                            let changed = !matches!(
                                (&j.status, &stage),
                                (JobStatus::Running(prev), new) if prev.label() == new.label()
                            );
                            if changed {
                                j.stage_started = std::time::Instant::now();
                            }
                            j.real_progress = frac;
                            j.status = JobStatus::Running(stage);
                        }
                    }
                }
                JobMsg::Done(id, generation, path) => {
                    let mut finished = false;
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        if j.generation == generation {
                            j.cancel = None;
                            j.status = JobStatus::Done(path);
                            finished = true;
                        }
                    }
                    if finished {
                        self.maybe_beep(true);
                    }
                }
                JobMsg::Failed(id, generation, err) => {
                    let mut failed = false;
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        if j.generation == generation {
                            j.cancel = None;
                            j.status = JobStatus::Failed(err);
                            failed = true;
                        }
                    }
                    if failed {
                        self.maybe_beep(false);

                        // A run can disprove what the start-up probe believed:
                        // a whisperx that answers `--version` but dies loading
                        // its CUDA libraries counts as an engine until it has
                        // actually been tried. Re-probe on a failure, and if
                        // nothing usable is left, put the download in front of
                        // the user rather than leaving them to guess — without
                        // this, the cached `whisper_ok` means the window they
                        // need can never open again this session.
                        if !whisper_installed() {
                            self.whisper_ok = false;
                            self.whisper_window_open = true;
                        }
                    }
                }
                JobMsg::Cancelled(id, generation) => {
                    // The worker has finished shutting down. Clear its cancel
                    // token so the job (already back at Queued) becomes eligible
                    // for the scheduler to run again when a slot frees.
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        if j.generation == generation {
                            j.cancel = None;
                            j.real_progress = None;
                            if !matches!(
                                j.status,
                                JobStatus::Done(_) | JobStatus::Failed(_)
                            ) {
                                j.status = JobStatus::Queued;
                            }
                        }
                    }
                }
                JobMsg::Title(_, title) if !title_is_usable(&title) => {
                    // Undecodable text — keep showing the URL rather than boxes.
                }
                JobMsg::Title(id, title) => {
                    // Name the job, and backfill the same title onto its entry
                    // in Recent so the menu stops showing a bare URL.
                    let mut url = None;
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        j.title = Some(title.clone());
                        if let Source::Url(u) = &j.source {
                            url = Some(u.clone());
                        }
                    }
                    if let Some(url) = url {
                        if let Some(entry) = self.url_history.iter_mut().find(|e| e.url == url) {
                            entry.title = Some(title);
                        }
                    }
                }
            }
        }

        // Start/stop workers to match the concurrency limit and drain the queue.
        // Runs every frame; the repaint request at the end of `update` keeps
        // frames coming while any job is queued or running.
        self.schedule_jobs();

        while let Ok(msg) = self.dl_rx.try_recv() {
            use download::DownloadMsg;
            // Every arm checks the message's tool id against the one we
            // believe is active: a worker that was cancelled can still emit
            // a few messages before it notices, and those must not touch
            // state that now belongs to a different download.
            let active_id = self.dl_active.as_ref().map(|a| a.id);
            match msg {
                DownloadMsg::Progress(id, done, total) => {
                    if active_id == Some(id) {
                        self.dl_active = Some(ActiveDownload {
                            id,
                            phase: DlPhase::Downloading,
                            done,
                            total,
                        });
                    }
                }
                DownloadMsg::Verifying(id) => {
                    if let Some(active) = self.dl_active.as_mut() {
                        if active.id == id {
                            active.phase = DlPhase::Verifying;
                        }
                    }
                }
                DownloadMsg::Extracting(id, done, total) => {
                    if let Some(active) = self.dl_active.as_mut() {
                        if active.id == id {
                            active.phase = DlPhase::Extracting;
                            active.done = done;
                            // A total of 0 means the archive header couldn't be
                            // read; leave it unknown and the UI spins instead.
                            active.total = (total > 0).then_some(total);
                        }
                    }
                }
                DownloadMsg::Done(id, _) => {
                    if active_id == Some(id) {
                        self.dl_active = None;
                    }
                    // The engine just landed — refresh the cached flag so the
                    // window shows "installed" and Start all stops gating.
                    if id == download::ToolId::FasterWhisper {
                        self.whisper_ok = whisper_installed();
                    }
                }
                DownloadMsg::Cancelled(id) => {
                    if active_id == Some(id) {
                        self.dl_active = None;
                    }
                }
                DownloadMsg::Failed(id, err) => {
                    if active_id == Some(id) {
                        self.dl_active = None;
                    }
                    self.dl_error = Some(err);
                }
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.add_file_paths(dropped);
        }

        // With the OS frame gone there are no native resize borders either;
        // the app provides its own edge grips. Computed up front so the
        // title bar's drag handler can yield to a resize at the top edge.
        let resize_dir = resize_direction_at_pointer(ctx);

        // ---------- Custom title bar (the OS one is turned off) ----------
        egui::TopBottomPanel::top("titlebar")
            .exact_height(34.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().bg)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ctx, |ui| {
                // The whole strip is a drag handle: click-drag moves the
                // window, double-click toggles maximize — registered before
                // the buttons so they still win the hit-test on top of it.
                let bar_response = ui.interact(
                    ui.max_rect(),
                    egui::Id::new("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if bar_response.drag_started_by(egui::PointerButton::Primary)
                    && resize_dir.is_none()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                if bar_response.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                ui.horizontal_centered(|ui| {
                    // Room to breathe, and a size the OS bar never offered.
                    ui.label(
                        egui::RichText::new("Henry's Shadowing App")
                            .size(15.0)
                            .strong()
                            .color(theme::palette().text),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if titlebar_button(ui, WinButton::Close) {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if titlebar_button(ui, WinButton::Maximize) {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                        }
                        if titlebar_button(ui, WinButton::Minimize) {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    });
                });
            });

        // ---------- Top panel: source & settings ----------
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().bg)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .show(ctx, |ui| {
            // Source card: local files on the left, URL entry filling the rest.
            theme::card().show(ui, |ui| {
                theme::section_label(ui, "SOURCE");
                ui.horizontal(|ui| {
                    if ui
                        .button("Add files…")
                        .on_hover_text(
                            "Queue local audio files — or just drop them \
                             anywhere in this window.",
                        )
                        .clicked()
                    {
                        if let Some(paths) = rfd::FileDialog::new()
                            .add_filter("Audio", &["mp3", "wav", "m4a", "flac", "ogg", "aac"])
                            .pick_files()
                        {
                            self.add_file_paths(paths);
                        }
                    }

                    // Right-to-left so the URL field soaks up whatever width
                    // the buttons beside it leave over.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let has_history = !self.url_history.is_empty();
                        ui.add_enabled_ui(has_history, |ui| {
                            // Glyphs here must exist in egui's *proportional* font chain
                            // (Ubuntu-Light → NotoEmoji → emoji-icon-font). The obvious
                            // picks don't: ▾ U+25BE lives only in the monospace font and
                            // ✕ U+2715 / ✓ U+2713 are in none of them, so they render as
                            // empty boxes. ⏷ ✖ ✔ are the equivalents that do exist.
                            ui.menu_button("Recent ⏷", |ui| {
                                ui.set_max_width(420.0);
                                // Snapshot history; selecting one populates the URL
                                // input box but does NOT add a job — the user can
                                // then change parameters and click Add URL.
                                let history: Vec<UrlEntry> = self.url_history.clone();
                                let mut chosen: Option<String> = None;
                                for entry in history.iter() {
                                    // Just the video's title; the URL only when
                                    // there's no title to show (lookup failed or
                                    // still in flight) or the title needs glyphs
                                    // our fonts don't have and would render as
                                    // boxes. Full URL on hover either way.
                                    let label = match &entry.title {
                                        Some(title) if renders_cleanly(ui, title) => {
                                            ellipsize(title, 70)
                                        }
                                        _ => ellipsize(&entry.url, 70),
                                    };
                                    if ui.button(label).on_hover_text(&entry.url).clicked() {
                                        chosen = Some(entry.url.clone());
                                        ui.close();
                                    }
                                }
                                if let Some(c) = chosen {
                                    self.url_input = c;
                                }
                            })
                            .response
                            .on_hover_text(
                                "Recently-used URLs. Picking one fills the input \
                                 field; change parameters first if you want, then \
                                 click Add URL.",
                            );
                        });

                        let url_valid = !self.url_input.trim().is_empty();
                        let add_clicked = ui
                            .add_enabled(url_valid, egui::Button::new("Add URL"))
                            .clicked();

                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.url_input)
                                .hint_text("Paste a YouTube link…")
                                .margin(egui::Margin::symmetric(10, 6))
                                .desired_width(ui.available_width()),
                        );
                        let enter_pressed = response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if add_clicked || (enter_pressed && url_valid) {
                            self.submit_url_input();
                        }
                    });
                });
            });

            ui.add_space(8.0);

            // Set inside the settings closure, acted on after it: starting a
            // download borrows self mutably, which is not available in there.
            let mut model_download: Option<download::ToolId> = None;

            // Settings card: everything that shapes how clips are produced.
            theme::card().show(ui, |ui| {
                // Stretch to the panel's full width (the source card above gets
                // this for free from its expanding URL field).
                ui.set_min_width(ui.available_width());
                theme::section_label(ui, "SETTINGS");

                // Transcription accuracy + feature toggles.
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Accuracy").color(theme::palette().text_muted));
                    egui::ComboBox::from_id_salt("whisper_model")
                        .selected_text(self.whisper_model.label())
                        .show_ui(ui, |ui| {
                            for option in [
                                WhisperModel::Tiny,
                                WhisperModel::Base,
                                WhisperModel::Small,
                                WhisperModel::Medium,
                                WhisperModel::LargeV3,
                            ] {
                                ui.selectable_value(
                                    &mut self.whisper_model,
                                    option,
                                    option.label(),
                                );
                            }
                        })
                        .response
                        .on_hover_text(
                            "Whisper transcription model size.\n\
                             Larger = more accurate sentence boundaries, slower, more VRAM.\n\
                             Each model downloads on first use.",
                        );

                    // Say so when the chosen accuracy needs fetching, right next
                    // to the choice that caused it, rather than letting the job
                    // discover it later. The smallest ships with the app, so this
                    // stays out of the way until someone asks for more.
                    if pipeline::whispercpp_available() && !pipeline::model_ready(self.whisper_model)
                    {
                        let id = download::ToolId::WhisperModel(self.whisper_model);
                        if let Some(id) = download_button(ui, id, self.dl_active.is_some()) {
                            model_download = Some(id);
                        }
                    }

                    ui.add_space(8.0);

                    let strip_music = ui
                        .checkbox(&mut self.strip_music, "Strip music")
                        .on_hover_text(
                            "Use Demucs to isolate vocals before transcribing. \
                             Slow on CPU, fast on GPU.",
                        );

                    // Switching this on without Demucs would only fail later, mid-job,
                    // so verify now and refuse, opening the explainer window. Checked
                    // on the way on but not off — probing imports torch and takes a
                    // moment.
                    if strip_music.changed() && self.strip_music && !deps::check_demucs().is_ok() {
                        self.strip_music = false;
                        self.demucs_window_open = true;
                    }

                    ui.checkbox(&mut self.show_text, "Show text")
                        .on_hover_text(
                            "Burn each sentence's transcript onto the video while \
                             it plays (white text on black).",
                        );

                    let translate = ui
                        .checkbox(&mut self.translate_english, "English translation")
                        .on_hover_text(
                            "Also render an English translation and show it beneath \
                             the original text. Uses the same transcription engine, \
                             so it needs no extra download — but roughly doubles the \
                             transcription time.",
                        );

                    // Translation is a second pass of the same engine, so turning
                    // it on with the engine missing would only fail at Start all.
                    // Offer the (already-required) download right away instead, the
                    // same way the engine window does elsewhere.
                    if translate.changed()
                        && self.translate_english
                        && !self.whisper_ok
                        && !whisper_installed()
                    {
                        self.whisper_window_open = true;
                    }
                    // "Beep on finish" used to sit here, but it isn't a per-clip
                    // setting — it applies to every job at once. It now lives with
                    // the other global controls in the bottom bar.
                });

                // Progress for a model fetch, directly under the row that asked
                // for it. Every other download in the app is watched from the
                // engine window; a model is started from here, so without this
                // the only feedback is a button going grey — which for the 3 GB
                // model means minutes of looking like a hang.
                if matches!(
                    self.dl_active.as_ref().map(|a| a.id),
                    Some(download::ToolId::WhisperModel(_))
                ) {
                    self.ui_download_status(ui);
                }

                // Chunk/repeat/duration controls.
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Repeats").color(theme::palette().text_muted));
                    ui.add(
                        egui::DragValue::new(&mut self.repeat_count)
                            .speed(1)
                            .range(1..=10),
                    )
                    .on_hover_text(
                        "How many times each sentence plays in the output.",
                    );

                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Gap").color(theme::palette().text_muted));
                    ui.add(
                        egui::DragValue::new(&mut self.gap_ratio)
                            .speed(0.1)
                            .range(0.0..=10.0)
                            .fixed_decimals(2)
                            .suffix("×"),
                    )
                    .on_hover_text(
                        "Length of the silent gap between repeats, as a multiple \
                         of the sentence's own length. 1.5 = gap is 1.5× the \
                         sentence. 0 = no gap.",
                    );

                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Max Sentence Length").color(theme::palette().text_muted));
                    ui.add(
                        egui::DragValue::new(&mut self.max_chunk_seconds)
                            .speed(0.5)
                            .range(2.0..=60.0)
                            .fixed_decimals(1)
                            .suffix(" sec"),
                    )
                    .on_hover_text(
                        "Sentences longer than this get split into evenly-sized \
                         pieces, each cut on a pause in the speech. A sentence \
                         with no pause to cut on is left whole, so this is a \
                         target rather than a hard limit.",
                    );

                    ui.add_space(8.0);
                    ui.checkbox(&mut self.limit_duration, "Only process the first")
                        .on_hover_text(
                            "When on, only the first N minutes of audio are \
                             processed. Useful for testing on long videos.",
                        );

                    let dur_enabled = self.limit_duration;
                    ui.add_enabled_ui(dur_enabled, |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.max_duration_minutes)
                                .speed(0.5)
                                .range(0.5..=240.0)
                                .fixed_decimals(1)
                                .suffix(" min"),
                        );
                    });
                });

                // Cookies + output folder.
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Cookies").color(theme::palette().text_muted));
                    let selected_text = match &self.cookie_source {
                        CookieSource::File(p) => {
                            let name = p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "file".to_string());
                            format!("File: {name}")
                        }
                        other => other.label().to_string(),
                    };
                    egui::ComboBox::from_id_salt("cookie_source")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for option in [
                                CookieSource::None,
                                CookieSource::Chrome,
                                CookieSource::Brave,
                                CookieSource::Edge,
                                CookieSource::Firefox,
                                CookieSource::Safari,
                            ] {
                                ui.selectable_value(
                                    &mut self.cookie_source,
                                    option.clone(),
                                    option.label(),
                                );
                            }
                            ui.separator();
                            if ui.button("Choose cookies file…").clicked() {
                                let mut dlg = rfd::FileDialog::new()
                                    .add_filter("Cookies (Netscape)", &["txt"]);
                                // Default the dialog to ~/Documents on Windows
                                // so a typical "I saved my cookies.txt there"
                                // workflow is one click.
                                if let Some(home) = std::env::var_os("USERPROFILE")
                                    .or_else(|| std::env::var_os("HOME"))
                                {
                                    let docs = PathBuf::from(home).join("Documents");
                                    if docs.is_dir() {
                                        dlg = dlg.set_directory(docs);
                                    }
                                }
                                if let Some(path) = dlg.pick_file() {
                                    self.cookie_source = CookieSource::File(path);
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "How to send YouTube your login cookies.\n\
                             Browser entries read live from the browser's profile (browser may need to be closed).\n\
                             'Choose cookies file…' uses an exported cookies.txt — Chrome can stay open.",
                        );

                    ui.add_space(8.0);

                    ui.label(egui::RichText::new("Output Folder").color(theme::palette().text_muted));
                    let path_text = self.output_dir.display().to_string();
                    ui.label(
                        egui::RichText::new(ellipsize_start(&path_text, 48))
                            .monospace()
                            .color(theme::palette().text_muted),
                    )
                    .on_hover_text(&path_text);
                    if ui.button("Change…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_directory(&self.output_dir)
                            .pick_folder()
                        {
                            self.output_dir = dir;
                        }
                    }

                    ui.add_space(8.0);

                    // Color scheme picker — restyles the whole app on the spot.
                    ui.label(egui::RichText::new("Theme").color(theme::palette().text_muted));
                    let scheme_before = self.color_scheme;
                    egui::ComboBox::from_id_salt("color_scheme")
                        .selected_text(
                            theme::SCHEMES[self.color_scheme.min(theme::SCHEMES.len() - 1)]
                                .name,
                        )
                        .show_ui(ui, |ui| {
                            for (i, scheme) in theme::SCHEMES.iter().enumerate() {
                                ui.selectable_value(&mut self.color_scheme, i, scheme.name);
                            }
                        })
                        .response
                        .on_hover_text("Try a different color scheme; your pick is saved.");
                    if self.color_scheme != scheme_before {
                        theme::set_scheme(ctx, self.color_scheme);
                        // Keep the DWM window border in step with the scheme.
                        #[cfg(target_os = "windows")]
                        win_chrome::set_border_color(theme::palette().card_stroke);
                    }
                });
            });

            if let Some(id) = model_download {
                self.start_tool_download(id);
            }
        });

        // ---------- Bottom bar: global controls + the main actions ----------
        // Only the counts the buttons need: Start all is enabled by pending
        // work, Clear finished by finished work. Each row already shows its own
        // status, so there's no summary line.
        let pending_count = self
            .jobs
            .iter()
            .filter(|j| matches!(j.status, JobStatus::Pending))
            .count();
        let done_count = self
            .jobs
            .iter()
            .filter(|j| matches!(j.status, JobStatus::Done(_)))
            .count();
        let failed_count = self
            .jobs
            .iter()
            .filter(|j| matches!(j.status, JobStatus::Failed(_)))
            .count();

        egui::TopBottomPanel::bottom("actions")
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().bg)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                // One row: the app-wide controls on the left (they apply to
                // every clip, not one, so they don't belong in the per-clip
                // settings), the main actions right-aligned beside them.
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Concurrent jobs")
                            .color(theme::palette().text_muted),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.max_concurrent_jobs)
                            .speed(0.05)
                            .range(1..=MAX_CONCURRENT_LIMIT),
                    )
                    .on_hover_text(
                        "How many clips process at the same time. Lower this while \
                         jobs are running and the most recently started ones are \
                         cancelled and put back in the queue, to start again when a \
                         slot frees up.",
                    );

                    ui.add_space(14.0);

                    theme::toggle_switch(ui, &mut self.beep_on_finish, "Beep on finish")
                        .on_hover_text(
                            "Play a system sound as each job finishes or fails. \
                             Global: on, every job beeps; off, none do.",
                        );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::primary_button(ui, pending_count > 0, "Start all")
                            .on_hover_text("Process every pending clip")
                            .clicked()
                        {
                            // Transcription is required, so verify the engine before
                            // starting — a run without it only fails. Re-probe until it
                            // passes (so downloading the engine then pressing again
                            // works), then trust the cached flag for the session.
                            if self.whisper_ok || whisper_installed() {
                                self.whisper_ok = true;
                                self.start_pending();
                            } else {
                                self.whisper_window_open = true;
                            }
                        }

                        let has_finished = done_count + failed_count > 0;
                        if ui
                            .add_enabled(has_finished, egui::Button::new("Clear finished"))
                            .clicked()
                        {
                            self.jobs.retain(|j| !j.status.is_finished());
                        }
                    });
                });
            });

        self.whisper_window(ctx);
        self.demucs_window(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().bg)
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ctx, |ui| {
                if self.jobs.is_empty() {
                    // Empty state: a quiet invitation rather than a blank void.
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.30);
                        ui.label(egui::RichText::new("🎧").size(52.0).color(theme::palette().text_faint));
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("Nothing queued yet")
                                .heading()
                                .color(theme::palette().text_muted),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Drop audio files anywhere in this window,\n\
                                 click Add files…, or paste a YouTube link above.",
                            )
                            .color(theme::palette().text_faint),
                        );
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        let mut remove_id: Option<u64> = None;
                        let mut retry_id: Option<u64> = None;

                        for job in &self.jobs {
                            theme::card().show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // A colored status glyph anchors the row.
                                    let (glyph, color) = match &job.status {
                                        JobStatus::Pending => ("•", theme::palette().text_faint),
                                        JobStatus::Queued => ("•", theme::palette().info),
                                        JobStatus::Running(_) => ("•", theme::palette().accent),
                                        JobStatus::Done(_) => ("✔", theme::palette().success),
                                        JobStatus::Failed(_) => ("✖", theme::palette().error),
                                    };
                                    ui.label(egui::RichText::new(glyph).color(color));

                                    // A fetched title that our fonts can't draw
                                    // (CJK, Thai, …) would be a row of boxes —
                                    // fall back to the URL for those.
                                    let mut name = job.display_name();
                                    if !renders_cleanly(ui, &name) {
                                        if let Source::Url(u) = &job.source {
                                            name = ellipsize(u, 60);
                                        }
                                    }
                                    ui.label(egui::RichText::new(name).strong())
                                        .on_hover_text(match &job.source {
                                            Source::File(p) => p.display().to_string(),
                                            Source::Url(u) => u.clone(),
                                        });

                                    // Stage dots, tucked right of the name.
                                    if !job.planned_stages.is_empty() {
                                        ui.add_space(2.0);
                                        stage_dots(ui, &job.planned_stages, &job.status);
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            // Remove works for any status — for active
                                            // jobs the worker thread keeps running and
                                            // its result is silently dropped.
                                            if theme::ghost_button(ui, "✖")
                                                .on_hover_text("Remove from list")
                                                .clicked()
                                            {
                                                remove_id = Some(job.id);
                                            }

                                            match &job.status {
                                                JobStatus::Pending => {
                                                    theme::chip(
                                                        ui,
                                                        "Pending",
                                                        theme::palette().text_muted,
                                                    );
                                                }
                                                JobStatus::Queued => {
                                                    theme::chip(ui, "Queued", theme::palette().info);
                                                    ui.add(
                                                        egui::Spinner::new()
                                                            .size(14.0)
                                                            .color(theme::palette().info),
                                                    );
                                                }
                                                JobStatus::Running(_) => {
                                                    // The bar under the name says it all.
                                                }
                                                JobStatus::Done(path) => {
                                                    theme::chip(ui, "Done", theme::palette().success)
                                                        .on_hover_text(
                                                            path.display().to_string(),
                                                        );
                                                    if theme::ghost_button(
                                                        ui,
                                                        "Show in folder",
                                                    )
                                                    .clicked()
                                                    {
                                                        reveal_in_folder(path);
                                                    }
                                                }
                                                JobStatus::Failed(_) => {
                                                    theme::chip(ui, "Failed", theme::palette().error);
                                                    // Retry resets the job to Pending so
                                                    // the next "Start all" runs it again
                                                    // with the same source.
                                                    if theme::ghost_button(ui, "↻ Retry")
                                                        .on_hover_text("Reset to Pending")
                                                        .clicked()
                                                    {
                                                        retry_id = Some(job.id);
                                                    }
                                                }
                                            }
                                        },
                                    );
                                });

                                match &job.status {
                                    JobStatus::Running(stage) => {
                                        // How far the *current* stage has got: a
                                        // real reported fraction when the stage
                                        // provides one (downloading, transcribing,
                                        // splitting, encoding), otherwise the
                                        // time-based ease-out estimate.
                                        let stage_frac = match job.real_progress {
                                            Some(f) => f.clamp(0.0, 1.0),
                                            None => {
                                                let elapsed =
                                                    job.stage_started.elapsed().as_secs_f32();
                                                let estimate = stage_estimate_seconds(stage);
                                                estimated_progress(elapsed, estimate)
                                            }
                                        };
                                        // Fold that into overall job completion so
                                        // the bar doesn't reset each stage.
                                        let frac = overall_progress(
                                            &job.planned_stages,
                                            stage,
                                            stage_frac,
                                        );
                                        ui.add_space(2.0);
                                        theme::progress_bar(
                                            ui,
                                            frac,
                                            &format!(
                                                "{} · {:.0}%",
                                                stage.label(),
                                                frac * 100.0
                                            ),
                                        );
                                    }
                                    JobStatus::Failed(err) => {
                                        // The first line is usually the actionable
                                        // bit; full text on hover. (ellipsize, not
                                        // a byte slice: error text is often
                                        // non-ASCII and slicing it mid-character
                                        // panics.)
                                        let first_line =
                                            err.lines().next().unwrap_or(err.as_str());
                                        ui.label(
                                            egui::RichText::new(ellipsize(first_line, 120))
                                                .small()
                                                .color(theme::palette().text_muted),
                                        )
                                        .on_hover_text(err);
                                    }
                                    _ => {}
                                }
                            });
                            ui.add_space(6.0);
                        }

                        if let Some(id) = remove_id {
                            // If it's running, signal its worker to stop so the
                            // slot frees up instead of the thread grinding on in
                            // the background after the row is gone.
                            if let Some(j) = self.jobs.iter().find(|j| j.id == id) {
                                if let Some(token) = &j.cancel {
                                    token.store(true, Ordering::SeqCst);
                                }
                            }
                            self.jobs.retain(|j| j.id != id);
                        }
                        if let Some(id) = retry_id {
                            if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                                j.status = JobStatus::Pending;
                            }
                        }
                    });
            });

        // While files are being dragged over the window, dim everything and
        // show a drop hint — the whole window is one big drop target.
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering_files {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("file_drop_overlay"),
            ));
            let rect = ctx.screen_rect();
            painter.rect_filled(rect, egui::CornerRadius::ZERO, egui::Color32::from_black_alpha(150));
            painter.rect_stroke(
                rect.shrink(10.0),
                egui::CornerRadius::same(12),
                egui::Stroke::new(2.0, theme::palette().accent),
                egui::StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop to add to the queue",
                egui::FontId::proportional(22.0),
                // Fixed near-white: the veil behind it is dark in every
                // scheme, including the light one.
                egui::Color32::from_rgb(0xF5, 0xF7, 0xFA),
            );
        }

        // Act on the edge grips last, so the resize cursor beats whatever a
        // hovered widget set, and a press at the edge starts a native
        // drag-resize (the OS takes over from there).
        if let Some(dir) = resize_dir {
            use egui::viewport::ResizeDirection;
            let icon = match dir {
                ResizeDirection::North => egui::CursorIcon::ResizeNorth,
                ResizeDirection::South => egui::CursorIcon::ResizeSouth,
                ResizeDirection::East => egui::CursorIcon::ResizeEast,
                ResizeDirection::West => egui::CursorIcon::ResizeWest,
                ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
                ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
                ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
                ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
            };
            ctx.output_mut(|o| o.cursor_icon = icon);
            if ctx.input(|i| i.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
            }
        }

        if self.jobs.iter().any(|j| j.status.is_active()) || self.dl_active.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

/// Play a short system sound. Non-blocking — spawns the player and
/// returns immediately. Silently does nothing if no player is available.
fn play_beep(success: bool) {
    #[cfg(target_os = "macos")]
    {
        // macOS ships a generous library of short sounds at
        // /System/Library/Sounds/*.aiff. Glass = success, Basso = failure.
        let sound = if success { "Glass" } else { "Basso" };
        let path = format!("/System/Library/Sounds/{sound}.aiff");
        let _ = std::process::Command::new("afplay").arg(path).spawn();
    }

    #[cfg(target_os = "linux")]
    {
        // Try paplay (PulseAudio/PipeWire) then aplay (ALSA). Sound file
        // is best-effort — these freedesktop paths are common defaults.
        let sound = if success {
            "/usr/share/sounds/freedesktop/stereo/complete.oga"
        } else {
            "/usr/share/sounds/freedesktop/stereo/dialog-error.oga"
        };
        if std::process::Command::new("paplay")
            .arg(sound)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("aplay").arg(sound).spawn();
        }
    }

    #[cfg(target_os = "windows")]
    {
        // PowerShell's [console]::Beep takes (frequency, duration_ms).
        let arg = if success {
            "[console]::Beep(880,200)"
        } else {
            "[console]::Beep(220,400)"
        };
        // Must be no_window_command, not a bare Command: a plain spawn gives
        // PowerShell its own console, which flashes up a black window every
        // time a job finishes.
        let _ = pipeline::no_window_command("powershell")
            .args(["-NoProfile", "-Command", arg])
            .spawn();
    }

    // Other platforms: silently do nothing.
    let _ = success;
}

// ---------- Settings persistence ----------

/// Flat snapshot of user-tunable settings. Kept separate from `App` so that
/// adding ephemeral UI state to `App` doesn't accidentally start getting
/// persisted.
struct Settings {
    whisper_model: String,
    strip_music: bool,
    repeat_count: usize,
    gap_ratio: f64,
    max_chunk_seconds: f64,
    limit_duration: bool,
    max_duration_minutes: f64,
    show_text: bool,
    translate_english: bool,
    beep_on_finish: bool,
    max_concurrent_jobs: usize,
    /// Scheme *name*, not index — stable if schemes are ever reordered.
    color_scheme: String,
    cookie_source: String,
    cookie_file: String,
    output_dir: String,
    url_history: Vec<UrlEntry>,
}

impl Settings {
    fn from_app(app: &App) -> Self {
        let (cookie_source, cookie_file) = match &app.cookie_source {
            CookieSource::None => ("none".to_string(), String::new()),
            CookieSource::Chrome => ("chrome".to_string(), String::new()),
            CookieSource::Brave => ("brave".to_string(), String::new()),
            CookieSource::Edge => ("edge".to_string(), String::new()),
            CookieSource::Firefox => ("firefox".to_string(), String::new()),
            CookieSource::Safari => ("safari".to_string(), String::new()),
            CookieSource::File(p) => ("file".to_string(), p.display().to_string()),
        };
        Self {
            whisper_model: match app.whisper_model {
                WhisperModel::Tiny => "tiny",
                WhisperModel::Base => "base",
                WhisperModel::Small => "small",
                WhisperModel::Medium => "medium",
                WhisperModel::LargeV3 => "large-v3",
            }
            .to_string(),
            strip_music: app.strip_music,
            repeat_count: app.repeat_count,
            gap_ratio: app.gap_ratio,
            max_chunk_seconds: app.max_chunk_seconds,
            limit_duration: app.limit_duration,
            max_duration_minutes: app.max_duration_minutes,
            show_text: app.show_text,
            translate_english: app.translate_english,
            beep_on_finish: app.beep_on_finish,
            max_concurrent_jobs: app.max_concurrent_jobs,
            color_scheme: theme::SCHEMES[app.color_scheme.min(theme::SCHEMES.len() - 1)]
                .name
                .to_string(),
            cookie_source,
            cookie_file,
            output_dir: app.output_dir.display().to_string(),
            url_history: app.url_history.clone(),
        }
    }

    fn apply(&self, app: &mut App) {
        app.whisper_model = match self.whisper_model.as_str() {
            "tiny" => WhisperModel::Tiny,
            "base" => WhisperModel::Base,
            "medium" => WhisperModel::Medium,
            "large-v3" => WhisperModel::LargeV3,
            _ => WhisperModel::Small,
        };
        // Honour a saved "on" only if Demucs is still installed — it may have
        // been removed since. Same rule as the checkbox, so the setting can
        // never come back enabled without the tool behind it.
        app.strip_music = self.strip_music && deps::check_demucs().is_ok();
        app.repeat_count = self.repeat_count.clamp(1, 10);
        app.gap_ratio = self.gap_ratio.clamp(0.0, 10.0);
        app.max_chunk_seconds = self.max_chunk_seconds.clamp(2.0, 60.0);
        app.limit_duration = self.limit_duration;
        app.max_duration_minutes = self.max_duration_minutes.clamp(0.5, 240.0);
        app.show_text = self.show_text;
        app.translate_english = self.translate_english;
        app.beep_on_finish = self.beep_on_finish;
        app.max_concurrent_jobs = self
            .max_concurrent_jobs
            .clamp(1, MAX_CONCURRENT_LIMIT);
        app.color_scheme = theme::scheme_index_by_name(&self.color_scheme).unwrap_or(0);
        app.url_history = self.url_history.clone();
        app.url_history.truncate(MAX_URL_HISTORY);
        app.cookie_source = match self.cookie_source.as_str() {
            "chrome" => CookieSource::Chrome,
            "brave" => CookieSource::Brave,
            "edge" => CookieSource::Edge,
            "firefox" => CookieSource::Firefox,
            "safari" => CookieSource::Safari,
            "file" if !self.cookie_file.is_empty() => {
                CookieSource::File(PathBuf::from(&self.cookie_file))
            }
            _ => CookieSource::None,
        };
        let candidate = PathBuf::from(&self.output_dir);
        if candidate.is_dir() {
            app.output_dir = candidate;
        }
    }

    fn serialize(&self) -> String {
        // Simple key=value, one per line. Escape newlines in values just in
        // case (output paths usually don't have them, but cheap to be safe).
        let escape = |s: &str| s.replace('\\', "\\\\").replace('\n', "\\n");
        let mut out = String::new();
        out.push_str(&format!("whisper_model={}\n", escape(&self.whisper_model)));
        out.push_str(&format!("strip_music={}\n", self.strip_music));
        out.push_str(&format!("repeat_count={}\n", self.repeat_count));
        out.push_str(&format!("gap_ratio={}\n", self.gap_ratio));
        out.push_str(&format!("max_chunk_seconds={}\n", self.max_chunk_seconds));
        out.push_str(&format!("limit_duration={}\n", self.limit_duration));
        out.push_str(&format!(
            "max_duration_minutes={}\n",
            self.max_duration_minutes
        ));
        out.push_str(&format!("show_text={}\n", self.show_text));
        out.push_str(&format!(
            "translate_english={}\n",
            self.translate_english
        ));
        out.push_str(&format!("beep_on_finish={}\n", self.beep_on_finish));
        out.push_str(&format!(
            "max_concurrent_jobs={}\n",
            self.max_concurrent_jobs
        ));
        out.push_str(&format!("color_scheme={}\n", escape(&self.color_scheme)));
        out.push_str(&format!("cookie_source={}\n", escape(&self.cookie_source)));
        out.push_str(&format!("cookie_file={}\n", escape(&self.cookie_file)));
        out.push_str(&format!("output_dir={}\n", escape(&self.output_dir)));
        for (i, entry) in self.url_history.iter().enumerate() {
            out.push_str(&format!("url_history.{i}={}\n", escape(&entry.url)));
            // Titles are written under a parallel key, matched back up by index
            // on load. An entry whose title we never got just omits the line.
            if let Some(title) = &entry.title {
                out.push_str(&format!("url_title.{i}={}\n", escape(title)));
            }
        }
        out
    }

    fn deserialize(text: &str) -> Self {
        let mut out = Self {
            whisper_model: "small".to_string(),
            strip_music: false,
            repeat_count: DEFAULT_REPEAT_COUNT,
            gap_ratio: DEFAULT_GAP_RATIO,
            max_chunk_seconds: DEFAULT_MAX_CHUNK_SECONDS,
            limit_duration: false,
            max_duration_minutes: DEFAULT_MAX_DURATION_MINUTES,
            show_text: false,
            translate_english: false,
            beep_on_finish: true,
            max_concurrent_jobs: DEFAULT_MAX_CONCURRENT_JOBS,
            color_scheme: String::new(),
            cookie_source: "none".to_string(),
            cookie_file: String::new(),
            output_dir: String::new(),
            url_history: Vec::new(),
        };

        let unescape = |s: &str| s.replace("\\n", "\n").replace("\\\\", "\\");

        // BTreeMap so the entries come back out in index order regardless of
        // the order the lines appear in the file.
        let mut urls: std::collections::BTreeMap<usize, String> = Default::default();
        let mut titles: std::collections::BTreeMap<usize, String> = Default::default();

        for line in text.lines() {
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let val = unescape(val);
            match key {
                "whisper_model" => out.whisper_model = val,
                "strip_music" => {
                    if let Ok(v) = val.parse() {
                        out.strip_music = v;
                    }
                }
                "repeat_count" => {
                    if let Ok(v) = val.parse() {
                        out.repeat_count = v;
                    }
                }
                "gap_ratio" => {
                    if let Ok(v) = val.parse() {
                        out.gap_ratio = v;
                    }
                }
                "max_chunk_seconds" => {
                    if let Ok(v) = val.parse() {
                        out.max_chunk_seconds = v;
                    }
                }
                "limit_duration" => {
                    if let Ok(v) = val.parse() {
                        out.limit_duration = v;
                    }
                }
                "max_duration_minutes" => {
                    if let Ok(v) = val.parse() {
                        out.max_duration_minutes = v;
                    }
                }
                "show_text" => {
                    if let Ok(v) = val.parse() {
                        out.show_text = v;
                    }
                }
                "translate_english" => {
                    if let Ok(v) = val.parse() {
                        out.translate_english = v;
                    }
                }
                "beep_on_finish" => {
                    if let Ok(v) = val.parse() {
                        out.beep_on_finish = v;
                    }
                }
                "max_concurrent_jobs" => {
                    if let Ok(v) = val.parse() {
                        out.max_concurrent_jobs = v;
                    }
                }
                "color_scheme" => out.color_scheme = val,
                "cookie_source" => out.cookie_source = val,
                "cookie_file" => out.cookie_file = val,
                "output_dir" => out.output_dir = val,
                // URLs and their titles are written under parallel keys; collect
                // both by index and zip them once the whole file is read, so the
                // pairing survives a missing title line (or a reordered file).
                k if k.starts_with("url_history.") => {
                    if let Ok(i) = k["url_history.".len()..].parse::<usize>() {
                        if !val.is_empty() {
                            urls.insert(i, val);
                        }
                    }
                }
                k if k.starts_with("url_title.") => {
                    if let Ok(i) = k["url_title.".len()..].parse::<usize>() {
                        // Drop titles saved before yt-dlp emitted UTF-8: they
                        // carry U+FFFD and would render as boxes forever. The
                        // entry keeps its URL and re-fetches the title next time
                        // it's used.
                        if !val.is_empty() && title_is_usable(&val) {
                            titles.insert(i, val);
                        }
                    }
                }
                _ => {}
            }
        }

        out.url_history = urls
            .into_iter()
            .map(|(i, url)| UrlEntry {
                title: titles.get(&i).cloned(),
                url,
            })
            .collect();

        out
    }
}

fn settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata)
                .join("henrys_shadowing_app")
                .join("config.txt")
        });
    }

    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("henrys_shadowing_app")
                .join("config.txt")
        });
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        return std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("henrys_shadowing_app")
                .join("config.txt")
        });
    }
}

fn load_settings() -> Option<Settings> {
    let path = settings_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Settings::deserialize(&text))
}

fn save_settings(app: &App) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, Settings::from_app(app).serialize());
}

fn default_cookie_source() -> CookieSource {
    // If the user has a cookies file at the conventional path, prefer it —
    // it lets browsers stay open and is generally more reliable.
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let candidate = PathBuf::from(home).join("Documents").join("yt-cookies.txt");
        if candidate.is_file() {
            return CookieSource::File(candidate);
        }
    }

    if cfg!(target_os = "windows") || cfg!(target_os = "macos") {
        CookieSource::Chrome
    } else {
        CookieSource::None
    }
}

fn default_output_dir() -> PathBuf {
    // Prefer ~/Downloads (works on macOS, Windows, and most Linux setups
    // since freedesktop's XDG_DOWNLOAD_DIR defaults to it). Fall back to
    // the current working directory, then the temp dir.
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let downloads = PathBuf::from(home).join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir())
}

/// True if a transcription engine is available — the downloaded standalone or
/// a system whisperx. Spawns subprocesses, so call it on events (startup,
/// Start-all, download-done), never in the per-frame render loop.
fn whisper_installed() -> bool {
    deps::check_transcription().is_ok()
}

fn download_button(
    ui: &mut egui::Ui,
    id: download::ToolId,
    download_busy: bool,
) -> Option<download::ToolId> {
    let spec = download::spec(id);
    let label = format!("Download ({})", download::human_bytes(spec.approx_size));
    let response = theme::primary_button(ui, !download_busy, &label).on_hover_text(
        "Downloads a pinned, checksum-verified copy just for this app. \
         Nothing is added to PATH or installed system-wide.",
    );
    if response.clicked() {
        Some(id)
    } else {
        None
    }
}

fn main() -> eframe::Result<()> {
    // The window and taskbar icon while the app is running. The exe's own icon
    // — what Explorer and shortcuts show — is embedded separately at build time
    // from assets/app.ico (see build.rs). Both come from the same source PNG.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("the bundled icon is a valid png");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Window scaled up by UI_SCALE too, so the larger UI has the same
            // amount of room as before rather than getting cramped.
            .with_inner_size([820.0 * UI_SCALE, 600.0 * UI_SCALE])
            .with_min_inner_size([560.0 * UI_SCALE, 420.0 * UI_SCALE])
            // No OS title bar — the app draws its own themed one (see
            // `title_bar` in App::update), which handles dragging and the
            // minimize/maximize/close buttons itself. The title string and
            // icon still matter: the taskbar and Alt-Tab show them.
            .with_decorations(false)
            .with_title("Henry's Shadowing App")
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "Henry's Shadowing App",
        options,
        Box::new(|cc| {
            // Scale the whole UI up — text, buttons, and the glyph "icons"
            // (✖ ↻ ⏷ ✔) all ride on the zoom factor, so one setting enlarges
            // everything uniformly. Layered on top of the OS DPI scaling egui
            // already applies.
            cc.egui_ctx.set_zoom_factor(UI_SCALE);

            // Dependencies are checked lazily on the first "Start all" press,
            // not at startup, so the window opens straight to the queue.
            let app = App::default();

            // The app's whole look — palette, spacing, rounding — lives in
            // theme.rs; style it with the user's saved color scheme.
            theme::set_scheme(&cc.egui_ctx, app.color_scheme);

            // Bring back the window dressing the OS dropped along with the
            // title bar: rounded corners, drop shadow, and a themed border.
            #[cfg(target_os = "windows")]
            win_chrome::init(cc, theme::palette().card_stroke);

            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}
