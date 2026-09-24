//! The self-test trigger (`buffer --self-test-clip-after`), end to end.
//!
//! This is the verification-only way to take a clip **without synthesising input**: it
//! waits until the ring holds a full window, calls the same [`Recorder::clip_now`] the
//! `Ctrl+F8` hotkey calls (once), logs the trigger and `wrote …` lines the runbook's
//! criteria already read, and stops. The hotkey itself still needs Windows — what can be
//! tested off Windows is everything either side of it.
//!
//! The test drives the real command in-process ([`localplay_cli::run_buffer_with`]), with
//! the real engine, a real ffmpeg encoder and a real ring buffer, over the synthetic
//! capture/audio sources. `--dev-software-encoder` (libx264) is what makes that possible
//! on a host with no GPU encoder; it is reachable here because this package's dev
//! dependencies enable `test-encoders`, which no shipping build does.
//!
//! Gating: not `#![cfg(feature = "test-encoders")]`, deliberately — the suite's command
//! line enables that feature on the *encoder* package and a `cfg` gate here would compile
//! the file away and quietly drop the test (the same trap `post_roll.rs` documents). The
//! dev-dependency edges in this package's manifest guarantee it instead.
//!
//! Nothing in this file (or in the code it tests) sends, simulates or injects a keyboard
//! or mouse event; the trigger is an in-process call, and the assertion below pins the
//! log line that says so.

use localplay_cli::{BufferOptions, run_buffer_with};
use localplay_media::{FfmpegBinaries, MediaInfo};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The clip's configured window (`pre_seconds + post_seconds`), in ms.
const WINDOW_MS: u64 = 3_000;
/// What `--self-test-clip-after` asks for. Above the window on purpose, so the wait under
/// test is the *requested* one rather than the pre-roll floor (which the unit tests pin).
const SELF_TEST_AFTER_S: u64 = 4;

/// The config the run uses. Small and fast: the point is the trigger, not a soak.
const CONFIG: &str = r#"
[buffer]
pre_seconds = 2
post_seconds = 1
segment_time = 1
scratch_cap_bytes = 268435456
scratch_dir = ""

[encode]
vendor = "auto"
codec = "h264"
bitrate_kbps = 2000
fps = 10
output_size = ""

[audio]
enabled = true
source = "loopback"
codec = "aac"
bitrate_kbps = 64

[storage]
clips_dir = ""
max_total_bytes = 536870912
max_age_days = 1

[hotkeys]
clip = "Ctrl+F8"

[events]
lol_poll_enabled = false
gsi_port = 0
"#;

#[test]
fn the_self_test_trigger_writes_a_clip_through_the_hotkey_path() {
    let app_dir = tempfile::tempdir().expect("a scratch application data directory");
    std::fs::write(app_dir.path().join("config.toml"), CONFIG).expect("writing config.toml");

    let logs = capture_logs();
    let started = Instant::now();
    let result = run_buffer_with(BufferOptions {
        app_dir: app_dir.path().to_path_buf(),
        self_test_clip_after: Some(SELF_TEST_AFTER_S),
        dev_software_encoder: true,
        // The file decides the mode (its `[recorder] mode`), and the real sources are used:
        // this test is about the shipping path.
        mode: None,
        stub_sources: false,
    });
    result.expect("the self-test run must succeed");

    // It really waited for the requested hold: the run cannot have finished faster than the
    // media it had to buffer plus the post-roll.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(SELF_TEST_AFTER_S),
        "the run ended after {elapsed:?}, which is less than the {SELF_TEST_AFTER_S}s of \
         media it was told to accumulate — the trigger did not wait"
    );

    // Exactly one clip, in the configured clips directory, written by the real splice.
    let clips_dir = app_dir.path().join("clips");
    let clips: Vec<_> = std::fs::read_dir(&clips_dir)
        .expect("the clips directory exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("clip-") && std::path::Path::new(n).extension().is_some_and(|e| e == "mp4")
            })
        })
        .collect();
    assert_eq!(clips.len(), 1, "expected exactly one clip, got {clips:?}");
    let clip = &clips[0];

    // The clip is the configured window: pre-roll AND post-roll. The tolerance is wider
    // than the runbook's ±0.5 s because this run's `segment_time` is 1 s and the splice
    // cuts on the segment grid — what it must rule out is a clip that is only one side of
    // the window (≈1 s or ≈2 s) or empty.
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let info = MediaInfo::probe(&bin, clip).expect("the clip must be a readable media file");
    // The clip carries BOTH sides of the window, and that is what this must rule out — along
    // with an empty clip. It deliberately does not demand a tight fit to `WINDOW_MS`: the
    // post-roll is whatever the encoder writes inside its budget, so a loaded machine
    // legitimately produces a shorter clip (CI measured 2498ms of video for a 3066ms window in
    // the encoder suite). The lower bound is the failure this exists to catch — a clip that
    // stopped at the pre-roll, i.e. one side of the window rather than both.
    assert!(
        info.duration_ms >= WINDOW_MS * 3 / 4 && info.duration_ms <= WINDOW_MS + 1_500,
        "clip duration {}ms is not the configured window ({WINDOW_MS}ms ± segment grid): a clip \
         that carries only one side of it, or is empty, is the failure this rules out",
        info.duration_ms
    );
    let video = info.video.as_ref().expect("a video stream");
    let audio = info.audio.as_ref().expect("an audio stream");
    assert_eq!(video.codec, "h264", "the software encoder is libx264");
    assert_eq!(audio.codec, "aac");
    assert_eq!(audio.sample_rate, 48_000);
    assert_eq!(audio.channels, 2);

    // The lines the runbook reads, produced by the hotkey's own trigger path: the `hotkey
    // pressed:` trigger line (media time, wall time, drift) and the `wrote` line. The
    // self-test line next to them says what triggered it, so a reader cannot mistake it
    // for a keypress that never happened.
    let log = logs.lock().expect("the log sink is not poisoned").clone();
    assert!(
        log.contains("no keyboard or mouse input is sent, simulated or injected"),
        "the self-test line must say that no input is involved; log:\n{log}"
    );
    assert!(
        log.contains("self-test: the ring holds"),
        "the trigger must report that the pre-roll was satisfied; log:\n{log}"
    );
    assert!(
        log.contains("hotkey pressed: media=") && log.contains("drift "),
        "the trigger line must carry media time, wall time and drift; log:\n{log}"
    );
    assert!(
        log.contains("waiting for post-roll"),
        "the post-roll wait must be the one under test; log:\n{log}"
    );
    let wrote = log
        .lines()
        .find(|l| l.contains("wrote ") && l.contains("encoder=libx264"))
        .expect("a `wrote …` line naming the encoder");
    assert!(wrote.contains("bytes"), "the wrote line carries the size: {wrote}");
    assert!(
        log.contains(&clip.display().to_string()),
        "the wrote line must name the clip that is on disk; log:\n{log}"
    );
    // The hotkey is unchanged and still installed: the run does not replace the shipping
    // trigger, it calls the same engine entry point.
    assert!(
        log.contains("buffering 2s pre / 1s post at 10fps; press Ctrl+F8 to clip"),
        "the shipping startup line must be unchanged; log:\n{log}"
    );
}

/// The run's tracing output, for the assertions above.
///
/// Installed once per test process (a global subscriber can only be set once), into a
/// shared buffer. `debug` because the engine's status line is at that level.
fn capture_logs() -> Arc<Mutex<String>> {
    static LOGS: OnceLock<Arc<Mutex<String>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let sink = Arc::new(Mutex::new(String::new()));
        tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_ansi(false)
            .with_target(false)
            .with_writer(SinkWriter(Arc::clone(&sink)))
            .try_init()
            .expect("the test process installs one subscriber");
        sink
    })
    .clone()
}

/// `tracing_subscriber::fmt`'s writer: appends into the shared string.
#[derive(Clone)]
struct SinkWriter(Arc<Mutex<String>>);

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the log sink is not poisoned")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SinkWriter {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
