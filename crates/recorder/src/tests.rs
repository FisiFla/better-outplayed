//! The recorder's own tests.
//!
//! These are unit tests (a `#[cfg(test)]` module) rather than `tests/` integration tests on
//! purpose: the end-to-end test below drives the engine with the synthetic sources and a
//! real ffmpeg encoder, which means it needs `EncodeConfig::for_tests_software` — and in an
//! integration test the library is compiled *without* `cfg(test)`, so that path would only
//! exist if the package's own `test-encoders` feature were enabled on the command line.
//! The suite's command line enables the *encoder's* feature (`--features
//! localplay-encoder/test-encoders`), which is not this package's. Compiling this file into
//! the library's test build sidesteps that entirely, exactly as the encoder crate's own
//! software-encoder tests do.
//!
//! Needs ffmpeg on `PATH` with a working libx264, like the rest of the suite.

use super::*;
use localplay_events::EventKind;

/// The stub geometry the end-to-end test records at. Small and slow on purpose: this is
/// four seconds of real-time capture, and 64x48 at 10fps encodes it in well under a second.
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 10;
const SEGMENT_SECONDS: u64 = 1;
const PRE_SECONDS: u64 = 2;
const POST_SECONDS: u64 = 1;

/// The application data directory the end-to-end recordings write into.
///
/// A temp directory by default, removed when the test ends. Setting
/// `LOCALPLAY_TEST_CLIPS_DIR` keeps the recording there instead, which is how the
/// verification run in the recorder's runbook re-runs the test and ffprobes the clip it
/// produced by hand:
///
/// ```text
/// LOCALPLAY_TEST_CLIPS_DIR=$PWD/target/recorder-e2e \
///   cargo test -p localplay-recorder -- --nocapture a_recorder_records
/// ffprobe target/recorder-e2e/clips/clip-*.mp4
/// ```
fn application_data_dir(what: &str) -> (Option<tempfile::TempDir>, PathBuf) {
    match std::env::var_os("LOCALPLAY_TEST_CLIPS_DIR") {
        // One subdirectory per test: the tests run in parallel and each recorder owns its
        // scratch directory.
        Some(dir) => (None, PathBuf::from(dir).join(what)),
        None => {
            let dir = tempfile::tempdir().expect("a temp dir for the recording");
            let path = dir.path().to_path_buf();
            (Some(dir), path)
        }
    }
}

/// A configuration that records from the synthetic sources into a temp directory.
///
/// `dev_software_encoder` is true because this host has no hardware encoder: the test's
/// encoder is real ffmpeg either way, which is the part that matters (a real rawvideo
/// pipe, a real segment muxer, a real `-c copy` splice).
fn stub_config(app_data_dir: &Path) -> RecorderConfig {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    RecorderConfig {
        bin,
        app_data_dir: app_data_dir.to_path_buf(),
        buffer: BufferSection {
            pre_seconds: PRE_SECONDS,
            post_seconds: POST_SECONDS,
            segment_time: SEGMENT_SECONDS,
            scratch_cap_bytes: 1 << 30,
            scratch_dir: String::new(),
        },
        encode: EncodeSection {
            vendor: "auto".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 2_000,
            fps: FPS,
            output_size: String::new(),
        },
        storage: StorageSection {
            clips_dir: String::new(),
            max_total_bytes: 1 << 30,
            max_age_days: 365,
        },
        sources: Sources::Stub(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS }),
        dev_software_encoder: true,
    }
}

/// Poll `check` until it holds or `budget` elapses, returning the last status.
///
/// The pipeline is real-time paced — the stubs deliver frames on the wall clock and ffmpeg
/// finalises a segment only once a second of media has gone through it — so a test can
/// assert on a counter reaching a value, but never on it reaching that value instantly.
fn wait_for(recorder: &Recorder, budget: Duration, what: &str, check: impl Fn(&RecorderStatus) -> bool) -> RecorderStatus {
    let deadline = Instant::now() + budget;
    loop {
        let status = recorder.status();
        if check(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "gave up waiting for {what}: {status:?} (error: {:?})",
            status.error
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_recorder_records_produces_a_clip_and_stops_cleanly() {
    let (_tmp, app_data_dir) = application_data_dir("record-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start(cfg).expect("the recorder starts");

    // 1. The status advances while nothing is asked of it: frames reach the encoder, and
    //    the ring's media timeline grows as ffmpeg finalises segments.
    let first = wait_for(&recorder, Duration::from_secs(30), "a non-empty buffer", |s| {
        s.frames > 0 && s.span_ms > 0
    });
    assert!(first.running, "the recorder must report itself as running");
    assert!(first.segments > 0, "a non-zero span implies a completed segment: {first:?}");
    assert!(first.bytes > 0, "the segments are on disk, so they have bytes: {first:?}");
    assert_eq!(first.configured_fps, FPS);
    assert_eq!(first.error, None, "nothing has failed");
    eprintln!("after the buffer filled: {first:?}");

    let span_before = first.span_ms;
    let second = wait_for(&recorder, Duration::from_secs(20), "the span to grow", |s| {
        s.span_ms > span_before
    });
    assert!(
        second.frames > first.frames,
        "frames must advance with the span: {} -> {}",
        first.frames,
        second.frames
    );
    eprintln!(
        "two seconds later: span {}ms -> {}ms, frames {} -> {}",
        first.span_ms, second.span_ms, first.frames, second.frames
    );

    // 2. The trigger. This is the media-time path: `clip_now` blocks until the post-roll
    //    is on disk, splices losslessly and indexes the result.
    let clip = recorder.clip_now().expect("the trigger must produce a clip");
    eprintln!(
        "clip_now() wrote {} ({}ms, {} bytes, encoder={}, indexed as {:?}, media t={}ms)",
        clip.metadata.path.display(),
        clip.metadata.duration_ms,
        clip.metadata.size_bytes,
        clip.metadata.encoder,
        clip.id,
        clip.started_at_ms
    );
    assert!(clip.metadata.path.is_file(), "the clip must exist on disk");
    assert!(clip.metadata.size_bytes > 0, "and must not be empty");
    assert_eq!(clip.metadata.size_bytes, std::fs::metadata(&clip.metadata.path).unwrap().len());
    assert_eq!(clip.metadata.encoder, "libx264", "the encoder that actually ran");
    // The window is `[trigger - pre, trigger + post]` in media terms and is spliced from
    // whole segments, so the file covers the request to within one segment — never less
    // (the post-roll is waited for), and never much more (whole segments, not the ring).
    let requested_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    assert!(
        clip.metadata.duration_ms >= requested_ms,
        "the clip must cover the full pre+post window: {}ms for a {requested_ms}ms request",
        clip.metadata.duration_ms
    );
    assert!(
        clip.metadata.duration_ms <= requested_ms + 2 * SEGMENT_SECONDS * 1000,
        "and must not be padded far past it: {}ms for a {requested_ms}ms request",
        clip.metadata.duration_ms
    );

    // Both streams, read back by ffprobe rather than assumed.
    let info = localplay_media::probe::MediaInfo::probe(&bin, &clip.metadata.path)
        .expect("the clip must be a readable media file");
    let video = info.video.as_ref().expect("the clip must carry video");
    let audio = info.audio.as_ref().expect("the clip must carry audio");
    eprintln!(
        "ffprobe: {}ms, {} bytes, video={} {}x{}, audio={} {}Hz {}ch",
        info.duration_ms,
        info.size_bytes,
        video.codec,
        video.width,
        video.height,
        audio.codec,
        audio.sample_rate,
        audio.channels
    );
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (WIDTH, HEIGHT));

    // 3. The clip is in the index, with the values the trigger produced — read back
    //    through a fresh connection so the test sees what a later run would see.
    let id = clip.id.expect("the clip was indexed");
    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let rows = store.list_clips().expect("list the index");
    assert_eq!(rows.len(), 1, "one clip, one row");
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.path, clip.metadata.path);
    assert_eq!(row.started_at_ms, clip.started_at_ms);
    assert_eq!(row.duration_ms, clip.metadata.duration_ms);
    assert_eq!(row.size_bytes, clip.metadata.size_bytes);
    assert_eq!(row.codec, "libx264");
    eprintln!("indexed row: {row:?}");

    // 4. The clean shutdown: the thread is joined, the encoder flushed, the sources
    //    stopped, and nothing panicked.
    recorder.stop().expect("stop must succeed");
    let stopped = recorder.status();
    assert!(!stopped.running, "the status must report the recorder stopped");
    assert!(stopped.clips >= 1, "the session counted its clip: {stopped:?}");
    assert_eq!(stopped.error, None, "a clean stop has no failure to report");
    eprintln!("after stop: {stopped:?}");

    // 5. Idempotence: a second stop, and a status read after it, must not panic or hang.
    recorder.stop().expect("a second stop is a no-op, not an error");
    assert!(!recorder.status().running);

    // 6. A trigger after the stop fails loudly rather than hanging for ever.
    let err = recorder.clip_now().expect_err("there is nothing left to clip");
    assert!(
        err.to_string().contains("not running") || err.to_string().contains("stopped"),
        "the error must say the recorder is gone: {err}"
    );
    eprintln!("clip_now() after stop: {err}");
}

#[test]
fn a_clip_records_why_it_was_taken() {
    // The wiring of a game event into the *same* trigger path the hotkey uses, end to end
    // and through the real database: one recording session, three different triggers.
    //
    //   1. a manual clip — the hotkey's path, which records no `events` row;
    //   2. a game event — the same path with a reason, which records one linked to the clip;
    //   3. a marker (a round boundary) — recorded, with no clip at all.
    let (_tmp, app_data_dir) = application_data_dir("event-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let ready_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(&recorder, Duration::from_secs(30), "enough media for a clip", |s| {
        s.span_ms >= ready_ms
    });

    // 1. Manual: exactly what the CLI does on a hotkey press.
    let manual = recorder.clip_now().expect("the manual trigger produces a clip");

    // 2. An event: the derived reason a driver would hand over, through the same call the
    //    event source's driver makes.
    let kill = GameEvent::new(
        localplay_events::Source::Lol,
        EventKind::Kill,
        serde_json::json!({ "source": "lol", "event": "ChampionKill", "killer": "Ahri" }),
    );
    let evented = recorder
        .clip_now_with(ClipReason::GameEvent(kill.clone()))
        .expect("the event trigger produces a clip too");

    // 3. A marker: no clip, just the row.
    let marker_id = recorder
        .note_event(GameEvent::bare(localplay_events::Source::Gsi, EventKind::RoundStart))
        .expect("the marker is recorded");

    recorder.stop().expect("stop must succeed");

    // Read the result back through a fresh connection, i.e. as a later run would see it.
    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let clips = store.list_clips().expect("list the clips");
    assert_eq!(clips.len(), 2, "both triggers wrote a clip: {clips:?}");

    let events = store.list_events().expect("list the events");
    assert_eq!(
        events.len(),
        2,
        "the manual clip wrote no event row, the event trigger wrote one, and the marker \
         wrote one: {events:?}"
    );

    let kill_row = events.iter().find(|e| e.kind == "kill").expect("the kill's row");
    assert_eq!(kill_row.clip_id, evented.id, "linked to the clip it produced");
    assert_eq!(
        kill_row.at_ms,
        evented.started_at_ms + PRE_SECONDS * 1000,
        "the event's `at` is the trigger instant, which is `pre_seconds` into the clip \
         (the clip starts earlier than the moment that caused it)"
    );
    assert!(
        kill_row.payload.as_deref().unwrap_or("").contains("ChampionKill"),
        "the integration's detail is persisted verbatim: {:?}",
        kill_row.payload
    );
    assert_eq!(kill_row.session_id, None, "this engine opens no session row");
    eprintln!("event row: {kill_row:?}");

    let marker_row = events.iter().find(|e| e.id == marker_id).expect("the marker's row");
    assert_eq!(marker_row.kind, "round_start");
    assert_eq!(marker_row.clip_id, None, "a marker has no clip");
    assert_eq!(marker_row.payload, None);
    assert!(
        marker_row.at_ms > kill_row.at_ms,
        "the marker is later on the media timeline: {} vs {}",
        marker_row.at_ms,
        kill_row.at_ms
    );

    assert!(
        events.iter().all(|e| e.clip_id != manual.id),
        "a manual clip is not an event, and writes no events row"
    );

    // The clip the event produced is a real clip — the reason did not change the media
    // path, only what was recorded about it.
    let info = localplay_media::probe::MediaInfo::probe(&recorder_bin(&app_data_dir), &evented.metadata.path)
        .expect("the event-triggered clip is a readable media file");
    assert!(info.video.is_some() && info.audio.is_some(), "video and audio, as for a hotkey clip");
    assert!(
        evented.metadata.duration_ms >= ready_ms,
        "and the same pre+post window: {}ms",
        evented.metadata.duration_ms
    );
}

/// The ffmpeg binaries the recorder used, for probing a clip after it stopped. Discovered
/// the same way the test's configuration discovers them.
fn recorder_bin(_app_data_dir: &Path) -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
}

#[test]
fn a_stopped_recorder_keeps_reporting_a_stable_status() {
    // The desktop shell polls `recording_status` from a timer and reads it either side of
    // a stop, so a status read after the loop is gone must be a value, not a panic and not
    // a hang. The counters are the ones the session reached: a stopped recorder is not a
    // zeroed one, and a UI that showed zeros would erase the evidence of what it recorded.
    let (_tmp, app_data_dir) = application_data_dir("stopped-status");
    let recorder = Recorder::start(stub_config(&app_data_dir)).expect("the recorder starts");
    wait_for(&recorder, Duration::from_secs(30), "the buffer to start", |s| {
        s.frames > 0
    });

    recorder.stop().expect("stop must succeed");
    let first = recorder.status();
    let second = recorder.status();
    assert_eq!(first, second, "two reads of a stopped recorder agree");
    assert!(!first.running);
    assert!(first.frames > 0, "the counters survive the stop: {first:?}");
}

#[test]
fn an_unusable_encoder_configuration_fails_before_any_capture_starts() {
    // The ordering that must not regress: the encoder is resolved (and smoke-tested)
    // before the capture backend exists. A configuration this machine cannot encode with
    // must therefore fail at `start`, and it must say which setting is wrong.
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = stub_config(dir.path());
    cfg.encode.codec = "vp9".to_string();

    let err = Recorder::start(cfg).map(|_| ()).expect_err("vp9 is not a clip codec");
    assert!(
        err.to_string().contains("unsupported encode.codec"),
        "the error must name the setting: {err}"
    );

    // A vendor that does not exist is rejected while resolving the encoder, i.e. before
    // any capture session exists. (The software-encoder path above skips vendor
    // resolution entirely — it needs no GPU — which is why this case clears the flag.)
    let mut cfg = stub_config(dir.path());
    cfg.dev_software_encoder = false;
    cfg.encode.vendor = "intel-arc".to_string();
    let err = Recorder::start(cfg).map(|_| ()).expect_err("that is not a vendor");
    assert!(err.to_string().contains("unknown encode.vendor"), "got: {err}");

    // And the observable consequence of the ordering: `RingBuffer::start` creates the
    // scratch directory, and it runs after the encoder is resolved and after the capture
    // backend exists. Neither failure above reached it, so nothing was ever captured.
    assert!(
        !dir.path().join("scratch").exists(),
        "the encoder must be resolved before the ring, and the ring before any capture"
    );
}

#[test]
fn a_recorder_reports_itself_as_not_running_before_it_is_started() {
    // What the desktop shell shows when nothing is recording. `configured_fps` is 0
    // because nothing has been configured — the UI says "not recording" rather than
    // inventing a rate.
    let idle = RecorderStatus::stopped();
    assert!(!idle.running);
    assert_eq!(idle.configured_fps, 0);
    assert_eq!(idle.frames, 0);
    assert_eq!(idle.error, None);
}
