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
            // Under `target/`, not the system temp: a test run leaves everything it wrote
            // inside the build directory (and removes it, unless it is killed).
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-tests");
            std::fs::create_dir_all(&base).expect("creating the test scratch root under target/");
            let dir = tempfile::Builder::new()
                .prefix(&format!("{what}-"))
                .tempdir_in(&base)
                .expect("a temp dir under target/");
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
            scratch_dir: String::new(),
            // Deliberately large: these tests are about the pipeline, and a ring that evicted
            // mid-recording would make them about eviction instead.
            ram_cap_bytes: 1 << 30,
        },
        encode: EncodeSection {
            vendor: "auto".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 2_000,
            fps: FPS,
            // The shipping default. At 64x48 libx264 measures far above 10fps, so the probe
            // cannot reduce the rate here — which is what the plain end-to-end test wants
            // (it is about the pipeline, not about adaptation).
            adapt_fps: true,
            output_size: String::new(),
        },
        storage: StorageSection {
            clips_dir: String::new(),
            max_total_bytes: 1 << 30,
            max_age_days: 365,
            sessions: crate::config::SessionStorageRules {
                sessions_dir: String::new(),
                max_total_bytes: 1 << 30,
                max_age_days: 365,
            },
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
    //
    //    The bar is the **window**, `pre + post`, and not merely "non-empty". The assertion at
    //    the end of this test is that a clip covers that window, and no window can be covered by
    //    footage that does not exist yet: a trigger at 1000ms with a 2000ms pre-roll asks for
    //    media from -1000ms, so the clip comes back one pre-roll short. That is exactly what
    //    happened the first time this suite ran against the in-memory ring — trigger=1000ms,
    //    window=[0ms, 2000ms), 2200ms of a 3000ms request.
    //
    //    Waiting for `span_ms > 0` used to be enough by accident. The file ledger's span lags a
    //    whole segment behind, because its newest file is untrusted until a later one appears, so
    //    "the span moved" meant two segments of footage had been written. A fragment in the
    //    in-memory ring carries its own length, so that ring's span is exact and the same wait
    //    returns after one segment. Asking for the window says what was meant all along, and is
    //    correct for both rings instead of relying on one of them being slow to count.
    let wanted_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let first = wait_for(&recorder, Duration::from_secs(30), "the window to be buffered", |s| {
        s.frames > 0 && s.span_ms >= wanted_ms
    });
    assert!(first.running, "the recorder must report itself as running");
    assert!(first.segments > 0, "a non-zero span implies a completed segment: {first:?}");
    assert!(first.bytes > 0, "the ring holds footage, so it has bytes: {first:?}");
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
    // whole segments, so the file covers the request to within one segment and never much
    // more (whole segments, not the ring).
    //
    // It can now be *shorter* than the request by one frame interval per segment, and that is
    // the honest shape of it rather than a defect: since the timeline fix
    // (`-fps_mode passthrough`, `localplay_encoder::ffmpeg`) a scratch segment holds the frames
    // that arrived while it was open — measured here at 10fps, ~940ms of media per 1000ms
    // segment, because the frame that would have crossed the boundary belongs to the next
    // segment and this one ends at the last frame that did arrive. Before the fix ffmpeg
    // resampled the arrival timestamps onto a 1/R grid, so every segment held exactly
    // `segment_time` of *invented* smoothness and the `span=` the ring read (segments ×
    // segment_time) was an over-estimate of footage that had to be waited for: the clip was
    // then the length it claimed and the footage in it was seconds old.
    let requested_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let frame_ms = 1000 / u64::from(FPS);
    let segments_in_window = requested_ms / (SEGMENT_SECONDS * 1000) + 1;
    let slack_ms = segments_in_window * frame_ms;
    assert!(
        clip.metadata.duration_ms + slack_ms >= requested_ms,
        "the clip must cover the pre+post window within one frame interval per segment \
         ({slack_ms}ms of slack): {}ms for a {requested_ms}ms request",
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
    // Three, not two: a manual clip used to write no event row at all, so a session's
    // timeline showed the game's kills and none of the moments the user chose. A hotkey clip
    // now records a `bookmark`, linked to the clip it produced.
    assert_eq!(
        events.len(),
        3,
        "the manual clip wrote a bookmark, the event trigger wrote a kill, and the marker \
         wrote a round_start: {events:?}"
    );

    // And all three belong to the session that was recording. This was NULL for every one of
    // them until the recorder started opening a `sessions` row; an event with no session is
    // now a *detached* event — what deleting its session leaves behind — rather than the
    // normal state of affairs.
    let session_id = events[0].session_id;
    assert!(
        session_id.is_some(),
        "an event is linked to the session that recorded it: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.session_id == session_id),
        "all three events belong to the one recording session: {events:?}"
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

    // The user's own marker. It is tagged with the same vocabulary the scrubber colours by,
    // points at the clip the hotkey produced, and carries no payload — a bookmark has no
    // source to describe, and inventing one would make the timeline claim a provenance it
    // does not have.
    let bookmark = events
        .iter()
        .find(|e| e.kind == BOOKMARK_KIND)
        .expect("the manual clip recorded a bookmark");
    assert_eq!(bookmark.clip_id, manual.id, "linked to the hotkey's own clip");
    assert!(bookmark.payload.is_none(), "a bookmark has no source to describe");
    assert_eq!(bookmark.session_id, session_id, "and it belongs to the same session");

    // The clip the event produced is a real clip — the reason did not change the media
    // path, only what was recorded about it.
    let info = localplay_media::probe::MediaInfo::probe(&recorder_bin(&app_data_dir), &evented.metadata.path)
        .expect("the event-triggered clip is a readable media file");
    assert!(info.video.is_some() && info.audio.is_some(), "video and audio, as for a hotkey clip");
    // The clip must carry the whole **pre-roll**: that footage was already on disk when the
    // trigger fired, and the window only reached back `pre_ms` for it, so a shorter clip would
    // mean the splice dropped footage that existed.
    //
    // It is deliberately NOT asserted against the full `ready_ms` window, which is what this
    // used to do. The post-roll is whatever the machine could write inside its budget
    // (`post_ms` + `POST_ROLL_MARGIN`), and on a loaded CI runner that legitimately runs out:
    // measured 2680ms of a 3000ms window, on a run whose recorder tests took 30.7s against the
    // usual 8.5s. That is the pipeline honouring its budget, not a lost clip — and asserting
    // the full window made this test fail for how busy the runner was.
    assert!(
        evented.metadata.duration_ms >= PRE_SECONDS * 1000,
        "the clip must cover the pre-roll that was already on disk: {}ms for a {PRE_SECONDS}s \
         pre-roll (the full window asked for {ready_ms}ms)",
        evented.metadata.duration_ms
    );
}

/// The ffmpeg binaries the recorder used, for probing a clip after it stopped. Discovered
/// the same way the test's configuration discovers them.
fn recorder_bin(_app_data_dir: &Path) -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
}

/// The regression test for issues #1 and #2: **the pacer and the encoder child are given the
/// same rate, and it is the rate the probe measured.**
///
/// This is the whole defect in one test. The pipeline used to declare the *configured*
/// `encode.fps` in two places at once — the child's `-framerate` and the pacer — on a machine
/// that could not encode it: the surplus went to the encoder's bounded queue and was dropped
/// (~45% of delivered frames on the measured 4K box, issue #1), and the media timeline ran at
/// a fraction of real time, so a configured `pre_seconds` of footage covered more real
/// seconds than asked for (issue #2).
///
/// The shape here is that failure, made deterministic: `encode.fps = 120` configured, a
/// measurement of 10, and a capture source that offers 120fps — so the pacer's rate is
/// visible in what reaches the encoder. Every assertion below fails if either consumer
/// recomputes its rate from `encode.fps` instead of taking the decided one:
///
/// * `effective_fps` is published from the **encoder child's own** `-framerate` (read back
///   through `Encoder::input_fps`), and the pacer is built from that same read-back, so the
///   two cannot be given different numbers without this assertion noticing;
/// * `frames` must advance at the *pacer's* rate, which is only observable because the source
///   offers more than it: a pacer at 120 would admit 120/s;
/// * nothing may be dropped, which is what "declare what you can deliver" buys.
#[test]
fn the_pacer_and_the_encoder_are_told_the_same_rate() {
    let (_tmp, app_data_dir) = application_data_dir("rate-agreement");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 120;
    cfg.encode.adapt_fps = true;
    // The source offers the configured rate; the pipeline may only keep what it declared.
    cfg.sources = Sources::Stub(StubConfig { width: WIDTH, height: HEIGHT, fps: 120 });

    // The measurement a 4K box produced: well below the configured rate.
    let measured_fps = 10.0;
    let recorder = Recorder::start_with_measure(cfg, &|encode_cfg: &EncodeConfig| {
        assert_eq!(
            encode_cfg.fps, 120,
            "the probe is handed the configuration that declares the *configured* rate: the \
             number being measured is part of the ffmpeg invocation"
        );
        Ok(ThroughputMeasurement {
            fps: measured_fps,
            frames: 15,
            window: Duration::from_millis(1500),
            source_size: (WIDTH, HEIGHT),
            output_size: (WIDTH, HEIGHT),
        })
    })
    .expect("the recorder starts");

    let status = recorder.status();
    assert_eq!(status.configured_fps, 120, "what the config asked for is still reported");
    assert_eq!(
        status.effective_fps, 10,
        "the pipeline must run at the measured 10fps: 10.0 floored is 10, and the encoder \
         child's own -framerate is where this value comes from"
    );

    // What the pacer admits, watched from outside: the source offers 120fps, so the number of
    // frames reaching the encoder per second *is* the pacer's rate.
    let first = wait_for(&recorder, Duration::from_secs(10), "a first rate sample", |s| s.frames > 0);
    let t0 = Instant::now();
    let second = wait_for(&recorder, Duration::from_secs(10), "two seconds of capture", |_| {
        Instant::now().duration_since(t0) >= Duration::from_millis(2000)
    });
    let wall = Instant::now().duration_since(t0).as_secs_f64();
    let submitted = (second.frames - first.frames) as f64;
    let observed = submitted / wall;
    eprintln!(
        "configured 120fps, measured {measured_fps}fps: {} frames over {wall:.2}s = \
         {observed:.1}/s (declared {}); skipped={} dropped={}",
        second.frames - first.frames,
        second.effective_fps,
        second.skipped,
        second.dropped
    );
    assert!(
        (5.0..=20.0).contains(&observed),
        "the pacer must admit the 10fps it declared, not the configured 120: {observed:.1}/s \
         over {wall:.2}s ({status:?})"
    );
    assert!(
        second.skipped > 0,
        "a source offering 120fps against a 10fps pacer is the case the readback skip exists \
         for: {second:?}"
    );
    assert_eq!(
        second.dropped, 0,
        "pacing to what the encoder was told is what stops the bounded queue from dropping: \
         {second:?}"
    );

    // The property the whole change is for: media time tracks the wall clock, so it must not
    // drift from it. (Issue #2's failure was `-f segment` resampling onto a declared grid,
    // which ran media at 0.66x wall.)
    //
    // `span_ms` is the newest segment's end offset, and a segment covers `SEGMENT_SECONDS`
    // of media, so span is a *staircase*, not a clock: it moves in whole-second steps. Waiting
    // a fixed wall-clock window and dividing therefore divides a staircase by a length, and
    // where the window lands on the staircase changes the answer. That is what made this fail
    // in CI (1000ms of media over 2096ms of wall) while passing locally: the window began
    // before ffmpeg had written its first segment, so the encoder's startup transient sat
    // inside the measurement and the quantised span moved exactly one step.
    //
    // So wait for conditions, not for durations: first for steady state (the first segment on
    // disk), then for a *defined amount of media*, and measure how long that took. Quantisation
    // is then absent from the ratio, because both sides are measured between the same two
    // media positions. A genuinely warped clock still fails: at 0.66x, producing
    // MEDIA_WINDOW_MS of media takes half again as long, putting the ratio near 0.66.
    const MEDIA_WINDOW_MS: u64 = 3_000;
    let steady =
        wait_for(&recorder, Duration::from_secs(15), "the first segment on disk", |s| s.span_ms > 0);
    let span_first = steady.span_ms;
    let t1 = Instant::now();
    let third = wait_for(&recorder, Duration::from_secs(30), "three more seconds of media", |s| {
        s.span_ms.saturating_sub(span_first) >= MEDIA_WINDOW_MS
    });
    let media = (third.span_ms.saturating_sub(span_first)) as f64;
    let wall = Instant::now().duration_since(t1).as_secs_f64() * 1000.0;
    eprintln!(
        "media time advanced {media:.0}ms over {wall:.0}ms of wall clock (from span \
         {span_first}ms)"
    );
    assert!(
        (0.75..=1.25).contains(&(media / wall)),
        "media time must track the wall clock (issue #2): {media}ms of media over {wall}ms of \
         wall clock"
    );

    recorder.stop().expect("stop must succeed");
}

/// `encode.adapt_fps = false` is the escape hatch: no probe runs at all (it would cost ~1.5s
/// of startup for a number the caller has said it does not want), and the configured rate is
/// declared exactly as it was before this change.
#[test]
fn with_adaptation_off_the_configured_rate_is_declared_and_nothing_is_measured() {
    let (_tmp, app_data_dir) = application_data_dir("rate-no-adaptation");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 60;
    cfg.encode.adapt_fps = false;

    let recorder = Recorder::start_with_measure(cfg, &|_: &EncodeConfig| {
        panic!("no measurement may be taken when encode.adapt_fps = false")
    })
    .expect("the recorder starts without measuring");

    let status = recorder.status();
    assert_eq!(status.configured_fps, 60);
    assert_eq!(status.effective_fps, 60, "the configured rate is declared as it always was");
    recorder.stop().expect("stop must succeed");
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

    // And the observable consequence of the ordering: the old file ring created the
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

// ---------------------------------------------------------------------------------------
// Phase 5: the mode, the session lifecycle, the microphone and the game watcher
// ---------------------------------------------------------------------------------------

use crate::session::layout_test_segments;
use localplay_events::process::{PresenceChange, WatchedGame};
use localplay_store::SESSION_MODE_SESSION;

/// The options this crate's tests build, so each test states only what it changes.
fn options(mode: RecordingMode, mic: bool) -> RecorderOptions {
    RecorderOptions {
        mode,
        mic: MicSection { enabled: mic },
        games: GamesSection::default(),
        game: None,
    }
}

/// The `(video, audio)` stream counts of a file, counted by ffprobe directly: `MediaInfo`
/// reports the *first* stream of each kind, which cannot tell one audio track from two.
fn stream_counts(path: &Path, bin: &FfmpegBinaries) -> (usize, usize) {
    let output = std::process::Command::new(&bin.ffprobe)
        .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("running ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let video = text.lines().filter(|line| line.trim() == "video").count();
    let audio = text.lines().filter(|line| line.trim() == "audio").count();
    (video, audio)
}

/// The session directories and the session files in an application data directory.
fn sessions_on_disk(app_data_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let sessions = app_data_dir.join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "mp4") {
                files.push(path);
            }
        }
    }
    (dirs, files)
}

/// Wait for the session store to hold exactly one finished session, and return its row.
///
/// The engine writes the row from its own connection while the test reads through a second
/// one, so the read is a poll rather than a single query.
fn wait_for_finished_session(bin: &FfmpegBinaries, db: &Path) -> localplay_store::Session {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let store = open_clip_index(db).expect("the index opens");
        let sessions = store.list_sessions().expect("listing sessions");
        if let Some(row) = sessions.iter().find(|row| row.ended_at_ms.is_some()) {
            return row.clone();
        }
        assert!(Instant::now() < deadline, "no finished session appeared: {sessions:?}");
        let _ = bin;
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_full_session_records_into_its_own_directory_and_keeps_every_segment() {
    // This used to be stated as "ignores the scratch cap", with the cap set to 1 KiB so that
    // enforcing it would visibly gut the session. There is no cap to ignore any more — the
    // file-backed ring that enforced one is gone — but the property it was standing in for is
    // still worth pinning, and is now stated directly: a session never evicts. It keeps its
    // segments as they age, which is the whole difference between it and a rolling buffer.
    let (_tmp, app_data_dir) = application_data_dir("full-session");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");

    let grown = wait_for(&recorder, Duration::from_secs(40), "four seconds of session", |s| {
        s.span_ms >= 4_000
    });
    assert_eq!(grown.mode, RecordingMode::FullSession);
    assert!(
        grown.segments >= 4,
        "four seconds of 1s segments must all still be there — a session evicts nothing: {grown:?}"
    );
    assert!(
        grown.bytes > 4 * 1_024,
        "and the footage is on disk: {grown:?}"
    );
    assert_eq!(grown.error, None, "nothing failed");

    // The segments are in a per-session directory under the sessions area, not in scratch/.
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert_eq!(dirs.len(), 1, "one session directory: {dirs:?}");
    assert!(files.is_empty(), "no session file exists until the recording stops: {files:?}");
    assert!(
        dirs[0].file_name().unwrap().to_string_lossy().starts_with("session-"),
        "named from the wall clock: {:?}",
        dirs[0]
    );
    let segments_in_dir = std::fs::read_dir(&dirs[0]).unwrap().filter_map(|e| e.ok()).count();
    assert!(segments_in_dir >= 4, "the segments are there: {segments_in_dir}");

    recorder.stop().expect("stop must succeed");

    // One finished session, one file, both streams, and the temporary segments gone.
    let row = wait_for_finished_session(&bin, &db_path);
    assert_eq!(row.mode, SESSION_MODE_SESSION);
    assert_eq!(row.game, None, "a manually started recording carries no game name");
    let final_path = row.final_path.clone().expect("the row names the session file");
    let final_path = PathBuf::from(&final_path);
    assert!(final_path.is_file(), "{final_path:?} exists");
    assert_eq!(stream_counts(&final_path, &bin), (1, 1), "one video, one audio track");
    assert_eq!(row.size_bytes as u64, std::fs::metadata(&final_path).unwrap().len());
    assert!(row.size_bytes > 4 * 1_024, "the session file is bigger than the cap: {row:?}");
    let info = localplay_media::probe::MediaInfo::probe(&bin, &final_path).expect("probing it");
    assert!(
        info.duration_ms >= 4_000,
        "the session file covers the whole session: {}ms",
        info.duration_ms
    );
    // The row's `duration_ms` is the file's own probed length — the axis a review timeline is
    // drawn against, written by the finalise rather than re-derived by a reader. Two
    // measurements of the same file, so the equality holds however loaded the machine was, and
    // it is what would catch the finalise recording the wall-clock window instead.
    assert_eq!(
        row.duration_ms, info.duration_ms as i64,
        "the session row must carry the recorded media's length, not the window it was \
         recorded in"
    );
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert!(dirs.is_empty(), "the temporary segments are removed: {dirs:?}");
    assert_eq!(files, vec![final_path], "and the session file is the only thing left");
}

#[test]
fn a_full_session_with_a_microphone_concatenates_both_audio_tracks() {
    let (_tmp, app_data_dir) = application_data_dir("session-mic");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    // `Sources::Stub` is what `stub_config` uses, so the microphone comes from
    // `StubMicrophone` — the only way this path can be covered on a machine with no WASAPI.
    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, true))
        .expect("the session recorder starts with a microphone");

    let status = recorder.status();
    assert!(status.mic, "the recording carries a microphone track");
    assert!(
        status.mic_port.is_some(),
        "and the encoder declared a second audio input: {status:?}"
    );
    assert_eq!(status.dropped_mic_audio, 0, "nothing has been dropped: {status:?}");

    let grown = wait_for(&recorder, Duration::from_secs(40), "three seconds of session", |s| {
        s.span_ms >= 3_000
    });
    assert_eq!(grown.dropped_mic_audio, 0, "the voice track is being fed: {grown:?}");
    assert_eq!(grown.error, None);

    // A clip taken during the session carries both tracks too: a session's own trigger knows
    // it has a microphone and splices with `-map 0` (which is also why the clip exists as a
    // separate check — the ring mode has no way to do this, and says so out loud at start).
    wait_for(&recorder, Duration::from_secs(40), "enough media for a clip", |s| {
        s.span_ms >= (PRE_SECONDS + POST_SECONDS) * 1000
    });
    let clip = recorder.clip_now().expect("the trigger must produce a clip");
    assert_eq!(
        stream_counts(&clip.metadata.path, &bin),
        (1, 2),
        "a clip out of a session with a microphone: {:?}",
        clip.metadata.path
    );

    recorder.stop().expect("stop must succeed");

    let row = wait_for_finished_session(&bin, &db_path);
    let final_path = PathBuf::from(row.final_path.expect("the session file"));
    // The point of the test: `-c copy` through a concat list is only lossless if the second
    // audio track survives it (`session::finalise`'s `-map 0`).
    assert_eq!(
        stream_counts(&final_path, &bin),
        (1, 2),
        "one video and **two** audio tracks in {final_path:?}"
    );
}

#[test]
fn a_clip_taken_during_a_full_session_is_the_same_instant_splice() {
    let (_tmp, app_data_dir) = application_data_dir("session-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    let ready_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(&recorder, Duration::from_secs(40), "enough media for a clip", |s| {
        s.span_ms >= ready_ms
    });

    // The hotkey's path, unchanged, in session mode: an instant clip out of the same
    // footage the session is being written into.
    let clip = recorder.clip_now().expect("the trigger must produce a clip");
    assert!(clip.metadata.path.is_file());
    assert_eq!(stream_counts(&clip.metadata.path, &bin), (1, 1));
    assert_eq!(clip.metadata.encoder, "libx264");
    assert!(clip.id.is_some(), "the clip is indexed");
    assert!(
        clip.metadata.path.starts_with(app_data_dir.join("clips")),
        "and it went to the clips directory, not the session's: {:?}",
        clip.metadata.path
    );

    recorder.stop().expect("stop must succeed");

    // Both artefacts survive: the clip and the session file.
    let row = wait_for_finished_session(&bin, &db_path);
    let session_file = PathBuf::from(row.final_path.expect("the session file"));
    assert!(session_file.is_file());
    assert!(clip.metadata.path.is_file());
    let store = open_clip_index(&db_path).expect("the index opens");
    assert_eq!(store.list_clips().unwrap().len(), 1, "one clip row");
    assert_eq!(store.list_sessions().unwrap().len(), 1, "one session row");
}

#[test]
fn the_session_row_is_opened_before_capture_and_kept_current_while_it_records() {
    let (_tmp, app_data_dir) = application_data_dir("session-row");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    wait_for(&recorder, Duration::from_secs(40), "three seconds of session", |s| s.span_ms >= 3_000);

    // Read the row through a second connection, exactly as the desktop shell's session list
    // would: opened, still running, with the bytes the tick has been reporting.
    let store = open_clip_index(&db_path).expect("the index opens");
    let rows = store.list_sessions().expect("listing sessions");
    assert_eq!(rows.len(), 1, "one session row: {rows:?}");
    let running = &rows[0];
    assert!(running.ended_at_ms.is_none(), "it is still recording");
    assert_eq!(running.mode, SESSION_MODE_SESSION);
    assert!(
        running.size_bytes > 0,
        "a multi-hour recording has to be visible to the retention cap before it stops: {running:?}"
    );
    assert!(
        Path::new(&running.scratch_dir).is_dir(),
        "the row names the directory the segments are in: {}",
        running.scratch_dir
    );
    let id = running.id;
    drop(store);

    recorder.stop().expect("stop must succeed");
    recorder.stop().expect("a second stop is a no-op, not an error");

    let store = open_clip_index(&db_path).expect("the index opens");
    let rows = store.list_sessions().unwrap();
    assert_eq!(rows.len(), 1, "a second stop does not open a second session");
    let finished = &rows[0];
    assert_eq!(finished.id, id);
    assert!(finished.ended_at_ms.is_some(), "the row is closed: {finished:?}");
    let path = PathBuf::from(finished.final_path.clone().expect("the session file"));
    assert!(path.is_file());
    assert_eq!(
        finished.size_bytes as u64,
        std::fs::metadata(&path).unwrap().len(),
        "and the size is the concatenated file's own"
    );
}

#[test]
fn the_microphone_is_off_by_default_and_leaves_the_encoder_invocation_unchanged() {
    let (_tmp, app_data_dir) = application_data_dir("mic-off");
    let cfg = stub_config(&app_data_dir);

    // The encoder's configuration is what its argument list is built from, and with the
    // microphone off it is exactly the pre-Phase-5 configuration: `mic_audio: None`, the
    // state whose argument list `localplay-encoder` pins byte for byte
    // (`the_argument_list_without_a_microphone_is_byte_identical_to_the_pre_change_list`).
    let encode_cfg = build_encode_config(
        &cfg,
        &cfg.scratch_dir(),
        localplay_encoder::VideoCodec::H264,
        None,
        (WIDTH, HEIGHT),
    )
    .expect("the encoder configuration");
    assert!(
        encode_cfg.mic_audio.is_none(),
        "no microphone means no second audio input (and no microphone URL in the args)"
    );

    // And the engine's own default is the microphone off — `Recorder::start` takes it.
    let options = RecorderOptions::default();
    assert!(!options.mic.enabled, "the microphone is opt-in");
    assert_eq!(options.mode, RecordingMode::ReplayBuffer, "and so is full-session mode");
    assert!(!options.games.auto_record, "and so is game-driven recording");

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let status = recorder.status();
    assert!(!status.mic);
    assert!(status.mic_port.is_none(), "the encoder opened no microphone input: {status:?}");
    assert_eq!(status.dropped_mic_audio, 0);
    wait_for(&recorder, Duration::from_secs(30), "the buffer to start", |s| s.span_ms > 0);
    assert_eq!(recorder.status().dropped_mic_audio, 0, "and stays 0 while recording");
    recorder.stop().expect("stop must succeed");
}

/// A microphone that is on but cannot start fails the recording **loudly**, before anything
/// is captured — never a video with a silent voice track.
///
/// Off Windows `Sources::Platform` has no microphone backend at all (the same rule
/// `localplay_capture::platform` applies to the video and audio backends), which is the
/// machine this runs on; on Windows the same code path reports a machine with no capture
/// endpoint, and the assertion about the display would have to be made there.
#[cfg(not(windows))]
#[test]
fn a_microphone_that_cannot_start_fails_the_recording_before_any_capture() {
    let (_tmp, app_data_dir) = application_data_dir("mic-unavailable");
    let mut cfg = stub_config(&app_data_dir);
    cfg.sources = Sources::Platform;
    let db_path = cfg.db_path();

    let err = Recorder::start_with_options(cfg, options(RecordingMode::ReplayBuffer, true))
        .map(|_| ())
        .expect_err("enabling the microphone on a platform without one must fail");

    let message = format!("{err:#}");
    assert!(
        message.contains("microphone"),
        "the error must name the microphone: {message}"
    );
    assert!(
        !app_data_dir.join("scratch").exists(),
        "nothing was captured: the capture backend is created after the microphone"
    );
    // The index was opened (it is the first step) but no session was opened for a recording
    // that never started.
    let store = open_clip_index(&db_path).expect("the index opens");
    assert!(store.list_sessions().expect("listing sessions").is_empty());
}

#[test]
fn auto_record_off_starts_no_watcher_and_records_immediately() {
    // The events crate's own contract, at the seam this crate uses: with `auto_record` false
    // `GamesSection::start` returns `None` — it starts nothing, not even a thread to gate.
    let (sink, _changes) = mpsc::channel();
    assert!(
        GamesSection::default().start(sink.clone()).expect("starting nothing cannot fail").is_none(),
        "auto_record = false must start nothing"
    );
    let off = GamesSection { watch: vec![WatchedGame::by_process("Dota 2", "dota2.exe")], ..GamesSection::default() };
    assert!(off.start(sink).expect("still nothing").is_none(), "nor with a watch list");

    // And the recorder: with the default options it is *recording*, not armed, which is only
    // possible because the watcher branch was not taken (`auto_record` is false, so no
    // channel, no receiver and no supervisor thread exist).
    let (_tmp, app_data_dir) = application_data_dir("games-off");
    let recorder = Recorder::start(stub_config(&app_data_dir)).expect("the recorder starts");
    let status = recorder.status();
    assert!(status.running, "a recorder with nothing watched records now: {status:?}");
    assert!(!status.watching_games, "and has no watcher: {status:?}");
    assert!(!recorder.is_armed());
    assert_eq!(status.game, None);
    recorder.stop().expect("stop must succeed");
}

#[test]
fn a_watched_game_starting_and_stopping_drives_a_full_session_and_the_retention_pass() {
    let (_tmp, app_data_dir) = application_data_dir("games-on");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 10;
    // A session-start rule of one day, and an old finished session that the retention pass
    // must therefore evict — while the session the game just wrote survives it.
    cfg.storage.sessions.max_age_days = 1;
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let sessions_dir = cfg.sessions_dir();
    let _ = &bin;
    std::fs::create_dir_all(&sessions_dir).expect("the sessions area");

    // The old session, with files on disk exactly as a real one leaves them.
    let old_dir = sessions_dir.join("session-old");
    std::fs::create_dir_all(&old_dir).expect("the old session directory");
    std::fs::write(old_dir.join("seg-000000.mp4"), vec![0u8; 512]).expect("an old segment");
    let old_file = sessions_dir.join("session-old.mp4");
    std::fs::write(&old_file, vec![0u8; 512]).expect("an old session file");
    let two_days_ms = 2 * 24 * 60 * 60 * 1_000i64;
    {
        let store = open_clip_index(&db_path).expect("the index opens");
        let id = store
            .start_session(Some("Dota 2"), SESSION_MODE_SESSION, now_ms() - two_days_ms, &old_dir.display().to_string(), 0)
            .expect("opening the old session");
        store
            .end_session(id, now_ms() - two_days_ms + 1_000, Some(&old_file.display().to_string()), 512, 0)
            .expect("ending the old session");
    }

    // The recorder is armed: nothing is captured until a game starts, and the injected
    // presence channel is the only way in (production's is `GamesSection::start`'s).
    let (presence, changes) = mpsc::channel();
    let game = WatchedGame::by_process("Dota 2", "dota2.exe");
    let recorder = Recorder::start_inner(
        cfg,
        options(RecordingMode::FullSession, false),
        // The supervisor builds the shipping probe itself; a recording it starts must not
        // call a caller's closure (which is what this panicking one proves).
        &|_cfg: &EncodeConfig| panic!("the injected measurement belongs to a recording `start` begins itself"),
        Some((changes, None)),
    )
    .expect("the armed recorder starts");

    let armed = recorder.status();
    assert!(!armed.running, "nothing is recorded before a game starts: {armed:?}");
    assert!(armed.watching_games && recorder.is_armed(), "but a watcher is running");
    assert_eq!(armed.game, None);
    assert_eq!(armed.error, None, "being armed is not an error");

    // The game starts: the recording begins, with the game's name in the session row.
    presence.send(PresenceChange::Started(game.clone())).expect("telling the recorder");
    let recording = wait_for(&recorder, Duration::from_secs(40), "the game's recording to start", |s| {
        s.running && s.span_ms >= 3_000
    });
    assert_eq!(recording.game.as_deref(), Some("Dota 2"), "{recording:?}");
    let store = open_clip_index(&db_path).expect("the index opens");
    let running: Vec<_> = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .filter(|row| row.ended_at_ms.is_none())
        .collect();
    assert_eq!(running.len(), 1, "one running session: {running:?}");
    assert_eq!(running[0].game.as_deref(), Some("Dota 2"), "carrying the detected title");
    drop(store);

    // The game stops: the recording is closed, the session file written, and the retention
    // pass runs — which is what removes the two-day-old session and nothing else.
    presence.send(PresenceChange::Stopped(game.clone())).expect("telling the recorder");
    wait_for(&recorder, Duration::from_secs(40), "the recording to stop", |s| !s.running);
    let stopped = recorder.status();
    assert_eq!(stopped.game, None, "no game is being recorded any more: {stopped:?}");
    assert!(stopped.watching_games, "but the watcher is still armed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let sessions = store.list_sessions().expect("listing sessions");
    assert_eq!(sessions.len(), 1, "the old session was evicted, the new one kept: {sessions:?}");
    let session = &sessions[0];
    assert_eq!(session.game.as_deref(), Some("Dota 2"));
    let file = PathBuf::from(session.final_path.clone().expect("the new session file"));
    assert!(file.is_file(), "the session the game produced is on disk: {file:?}");
    assert_eq!(stream_counts(&file, &bin), (1, 1));
    assert!(!old_file.exists(), "the retention pass removed the old session's file");
    assert!(!old_dir.exists(), "and its scratch directory");
    drop(store);

    // A second game starts, and this time the *application* shuts down mid-session: the
    // supervisor stops the recording it is running, which closes that session exactly as the
    // game stopping did (flush, session file, row ended).
    presence.send(PresenceChange::Started(game.clone())).expect("telling the recorder");
    wait_for(&recorder, Duration::from_secs(40), "the second game's recording", |s| {
        s.running && s.span_ms >= 3_000
    });
    recorder.stop().expect("stopping the recorder stops the watcher and its recording too");
    assert!(!recorder.is_armed(), "and leaves nothing armed");
    assert!(!recorder.is_running());
    let store = open_clip_index(&db_path).expect("the index opens");
    let sessions = store.list_sessions().expect("listing sessions");
    let other = sessions
        .iter()
        .find(|row| row.id != session.id)
        .expect("the second game's session row: {sessions:?}");
    assert!(other.ended_at_ms.is_some(), "stopping the application closed it: {other:?}");
    let file = PathBuf::from(other.final_path.clone().expect("with its session file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(stream_counts(&file, &bin), (1, 1));
}

#[test]
fn a_crashed_session_is_recovered_when_the_next_run_starts() {
    // A crash — a killed process — leaves a running `sessions` row and a directory of
    // segments with no session file. The next start must finish it, not orphan it.
    let (_tmp, app_data_dir) = application_data_dir("crash-recovery");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let sessions_dir = cfg.sessions_dir();
    std::fs::create_dir_all(&sessions_dir).expect("the sessions area");
    let session_dir = sessions_dir.join("session-1700000000");
    let segments = layout_test_segments(&session_dir, 1).expect("the crashed session's segments");
    let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    let id = {
        let store = open_clip_index(&db_path).expect("the index opens");
        let id = store
            .start_session(Some("League of Legends"), SESSION_MODE_SESSION, 1_700_000_000_000, &session_dir.display().to_string(), 0)
            .expect("the row the crash left behind");
        store.set_session_size(id, bytes as i64).expect("its last known size");
        id
    };

    // The next run — an ordinary buffer-mode recording, because recovery is not a session
    // mode feature: any start recovers what a previous one left.
    let recorder = Recorder::start(cfg).expect("the recorder starts");
    recorder.stop().expect("stop must succeed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let row = store.get_session(id).expect("reading the row").expect("the row exists");
    assert!(row.ended_at_ms.is_some(), "the crashed session was closed: {row:?}");
    let file = PathBuf::from(row.final_path.expect("and it names the recovered file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(stream_counts(&file, &bin), (1, 1), "with the streams its segments carried");
    assert_eq!(row.size_bytes as u64, std::fs::metadata(&file).unwrap().len());
    assert!(!session_dir.exists(), "and the temporary segments are gone");
}

#[test]
fn dropping_a_recorder_finalises_and_closes_its_session() {
    // The `Drop` path is a `stop()` — a front-end that forgets to stop, or panics on the way
    // out, must not leave a session row running forever or a directory of segments nobody
    // names. Dropping is the only shutdown this test performs.
    let (_tmp, app_data_dir) = application_data_dir("drop-session");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    wait_for(&recorder, Duration::from_secs(40), "three seconds of session", |s| s.span_ms >= 3_000);
    drop(recorder);

    let row = wait_for_finished_session(&bin, &db_path);
    assert!(row.ended_at_ms.is_some(), "the drop closed the session: {row:?}");
    let file = PathBuf::from(row.final_path.clone().expect("the session file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(stream_counts(&file, &bin), (1, 1));
    assert_eq!(row.size_bytes as u64, std::fs::metadata(&file).unwrap().len());
    let (dirs, _files) = sessions_on_disk(&app_data_dir);
    assert!(dirs.is_empty(), "and the temporary segments went with it: {dirs:?}");
}

/// Everything in `dir`, by name. An absent directory counts as empty, and that is the point
/// rather than a convenience: the strongest form of "buffering wrote nothing" is that the
/// directory `-f segment` used to write into was never created at all.
fn entries_in(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.expect("a directory entry").file_name().to_string_lossy().into_owned())
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!("reading {}: {err}", dir.display()),
    }
}

/// Constraint 1 of the in-memory buffer, asserted where it is actually claimed: **replay-buffer
/// mode writes nothing to disk while it is only buffering.**
///
/// The whole purpose of this mode is that unclipped footage never reaches the SSD. `-f segment`
/// used to write a file per second into `<app data dir>/scratch` and the filesystem ledger used
/// to delete the oldest one; if any of that came back, nothing else in this suite would notice.
/// The clips would still be correct, the status would still advance, and the only symptom would
/// be a disk doing work nobody asked it to — which is why this checks the thing directly rather
/// than inferring it from behaviour that is supposed to be identical either way.
///
/// The clips directory is checked too, because "the disk is only touched when a clip is saved"
/// has a second half: saving one must leave exactly one file behind.
#[test]
fn buffering_writes_nothing_to_disk_and_only_a_saved_clip_does() {
    let (_tmp, app_data_dir) = application_data_dir("ram-buffer-no-writes");
    let cfg = stub_config(&app_data_dir);
    let scratch = app_data_dir.join("scratch");
    // Where a clip lands, derived the way the recorder derives it (the directory the failed
    // runs above wrote into).
    let clips = app_data_dir.join("clips");

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let wanted_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let filled = wait_for(&recorder, Duration::from_secs(30), "the window to be buffered", |s| {
        s.frames > 0 && s.span_ms >= wanted_ms
    });
    assert!(filled.running, "the recorder must report itself as running");

    // Not a segment, not a partial file, not even the directory. The old file ring created it
    // and ffmpeg had written the first segment into it well inside this window.
    let written = entries_in(&scratch);
    assert!(
        written.is_empty(),
        "buffering wrote {written:?} into {} — unclipped footage must stay in RAM",
        scratch.display()
    );
    assert!(
        entries_in(&clips).is_empty(),
        "no clip has been asked for yet, so the clips directory must be empty"
    );

    // The trigger is the one thing that is allowed to touch the disk.
    let clip = recorder.clip_now().expect("the trigger must produce a clip");
    assert!(clip.metadata.path.is_file(), "the saved clip is on disk");
    assert!(entries_in(&scratch).is_empty(), "saving a clip left a scratch trail");

    let saved = entries_in(&clips);
    assert_eq!(
        saved.len(),
        1,
        "the clips directory must hold exactly the one saved clip, found {saved:?}"
    );
}
