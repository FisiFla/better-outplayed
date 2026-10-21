//! The self-test trigger (`buffer --self-test-clip-after`), end to end.
//!
//! The way to take a clip without synthesising input: it waits until the ring holds a full
//! window, calls the same [`Recorder::clip_now`] the `Ctrl+F8` hotkey calls (once), logs the
//! trigger and `wrote ...` lines, and stops. The hotkey itself needs Windows; everything either
//! side of it is tested here.
//!
//! Drives the real command in-process ([`replayed_cli::run_buffer_with`]) with the real engine,
//! a real ffmpeg encoder and a real ring over the synthetic capture/audio sources.
//! `--dev-software-encoder` (libx264) makes that possible on a host with no GPU encoder; it is
//! reachable because this package's dev dependencies enable `test-encoders`, which no shipping
//! build does.
//!
//! Gating: not `#![cfg(feature = "test-encoders")]`, deliberately: that feature is enabled on
//! the encoder package, so a `cfg` gate here would quietly drop the test (the trap `post_roll.rs`
//! documents).
//!
//! Nothing here sends, simulates or injects a keyboard or mouse event; the trigger is an
//! in-process call, and an assertion below pins the log line that says so.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use replayed_cli::{run_buffer_with, BufferOptions};
use replayed_media::{FfmpegBinaries, MediaInfo};
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
        // The file decides the mode (its `[recorder] mode`), and the real sources are used: this test
        // is about the shipping path.
        mode: None,
        stub_sources: false,
    });
    // A headless CI runner has no audio render endpoint, so the real audio source cannot start
    // there. An environment limit, not a defect: skip loudly rather than fake a device.
    if let Err(err) = &result {
        if format!("{err:#}").contains("GetDefaultAudioEndpoint") {
            eprintln!("SKIPPED: no default audio render endpoint on this machine: {err:#}");
            return;
        }
    }
    result.expect("the self-test run must succeed");

    // It really waited for the requested hold: the run cannot finish faster than the media it had
    // to buffer plus the post-roll.
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
                n.starts_with("clip-")
                    && std::path::Path::new(n)
                        .extension()
                        .is_some_and(|e| e == "mp4")
            })
        })
        .collect();
    assert_eq!(clips.len(), 1, "expected exactly one clip, got {clips:?}");
    let clip = &clips[0];

    // The clip is the configured window: pre-roll and post-roll. The tolerance is wider than 0.5 s
    // because `segment_time` is 1 s and the splice cuts on the segment grid; it must rule out a clip
    // of only one side of the window (~1 s or ~2 s) or an empty one.
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let info = MediaInfo::probe(&bin, clip).expect("the clip must be a readable media file");
    // The clip carries both sides of the window. It does not demand a tight fit to `WINDOW_MS`: the
    // post-roll is whatever the encoder writes inside its budget, so a loaded machine legitimately
    // produces a shorter clip (CI measured 2498ms of video for a 3066ms window). The lower bound
    // catches a clip that stopped at the pre-roll.
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

    // The lines produced by the hotkey's own trigger path: the `hotkey pressed:` line (media time,
    // wall time, drift) and the `wrote` line. The self-test line beside them says what triggered
    // it, so it cannot be mistaken for a keypress.
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
    assert!(
        wrote.contains("bytes"),
        "the wrote line carries the size: {wrote}"
    );
    assert!(
        log.contains(&clip.display().to_string()),
        "the wrote line must name the clip that is on disk; log:\n{log}"
    );
    // The hotkey is unchanged and still installed: the run calls the same engine entry point and
    // does not replace the shipping trigger.
    assert!(
        log.contains("buffering 2s pre / 1s post at 10fps; press Ctrl+F8 to clip"),
        "the shipping startup line must be unchanged; log:\n{log}"
    );
}

/// The run's tracing output, for the assertions above.
///
/// Installed once per test process (a global subscriber can be set once) into a shared buffer;
/// `debug` because the engine's status line is at that level.
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

/// Where the test's log is written, so that a process which dies of an access violation leaves
/// behind the last thing it was doing.
///
/// `REPLAYED_TEST_LOG` names the file when the caller wants a known path — a CI job that
/// uploads it, say. Otherwise the per-test temporary directory cargo provides is used, which
/// exists for exactly this kind of scratch.
fn log_file() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("REPLAYED_TEST_LOG") {
        return Some(std::path::PathBuf::from(path));
    }
    Some(
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("self_test_clip.log"),
    )
}

/// `tracing_subscriber::fmt`'s writer: appends into the shared string.
#[derive(Clone)]
struct SinkWriter(Arc<Mutex<String>>);

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0
            .lock()
            .expect("the log sink is not poisoned")
            .push_str(&text);
        // The sink is a string in this process, and a process that dies of an access violation
        // takes its memory with it: the last thing it logged would be lost with everything
        // else. So the same lines go to stderr, which the test runner keeps whatever happens,
        // and to a file, which survives even the runner losing the child's output.
        eprint!("{text}");
        if let Some(path) = log_file() {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = file.write_all(text.as_bytes());
            }
        }
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
