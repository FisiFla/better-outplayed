//! The shell's side of the recording engine: the recorder slot and its status DTOs.

use crate::config::{EngineOptionsConfig, RecordingConfig};
use localplay_media::FfmpegBinaries;
use localplay_recorder::event_driver::{EventDriver, Policy};
use localplay_recorder::{RecordedClip, Recorder, RecorderConfig, RecorderStatus, Sources};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
#[cfg(test)]
use localplay_capture::stub::StubConfig;
use super::*;
use super::settings::recorder_options;

// ---------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------
//
// The engine is `localplay-recorder`, the one the CLI drives. Here: one slot that holds the
// recording this window started, and the plain functions the `#[tauri::command]` wrappers
// delegate to.
//
// * `recording_status` never blocks the recording loop. The engine publishes its counters
//   through atomics (`Recorder::status`), so a poll every 500ms is a handful of loads and
//   cannot disturb a capture keeping up with 60fps.
// * A clip trigger does block: `clip_now` waits for the post-roll to be written
//   (`post_seconds` of media plus a margin). The slot lock is released before the wait: the
//   caller clones a handle out of the slot and blocks on that, so the window's status poll keeps
//   answering while a clip is being saved.

/// The live recording status, as the frontend sees it.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the exact
/// JSON keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatusDto {
    /// Whether a recording is running. `false` also means "never started".
    pub running: bool,
    /// Video frames submitted to the encoder this session.
    pub frames: u64,
    /// Completed segments in the scratch ring.
    pub segments: u64,
    /// Bytes the ring holds on disk.
    pub bytes: u64,
    /// Media time on disk, in ms: how much footage a clip can be cut from.
    pub span_ms: u64,
    /// Encoder frames dropped because its queue was full (the machine cannot keep up).
    pub dropped: u64,
    /// The same for audio blocks.
    pub dropped_audio: u64,
    /// Frames the capture backend offered and the pacer skipped without reading back.
    pub skipped: u64,
    /// The achieved frame rate over the last second; 0.0 before one has been measured.
    pub fps: f64,
    /// What `encode.fps` asked for, to show the two side by side.
    pub configured_fps: u32,
    /// The rate the pipeline is actually running at: the pacer's rate and the encoder child's
    /// `-framerate`, one number chosen at startup from what the throughput probe measured. Equal to
    /// `configured_fps` unless this machine could not hold the configured rate at the captured
    /// resolution (the engine has logged why). It is the rate the media timeline is recorded at, so
    /// an achieved-rate readout belongs next to it.
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the media timeline is behind
    /// real time, which is why a trigger is taken from `span_ms` rather than the clock.
    pub drift_ms: i64,
    /// Clips this session has written.
    pub clips: u64,
    /// Why the engine stopped, when it stopped for a failure rather than a `stop`.
    pub error: Option<String>,
    /// Whether a game watcher is armed for this recorder: `false` unless `[games] auto_record` was
    /// on at start. True with `running` false means armed and waiting for a game.
    pub watching_games: bool,
    /// The game being recorded, when a watched game triggered it. `None` while nothing
    /// is being recorded, and for a recording a person started.
    pub matched_game: Option<String>,
    /// The mode this recorder was started in (`"buffer"` or `"session"`).
    pub recorder_mode: String,
    /// Whether this recording carries a microphone track (`[mic] enabled`).
    pub mic_enabled: bool,
}

impl From<RecorderStatus> for RecordingStatusDto {
    fn from(status: RecorderStatus) -> Self {
        Self {
            running: status.running,
            frames: status.frames,
            segments: status.segments,
            bytes: status.bytes,
            span_ms: status.span_ms,
            dropped: status.dropped,
            dropped_audio: status.dropped_audio,
            skipped: status.skipped,
            fps: status.fps,
            configured_fps: status.configured_fps,
            effective_fps: status.effective_fps,
            drift_ms: status.drift_ms,
            clips: status.clips,
            error: status.error,
            watching_games: status.watching_games,
            matched_game: status.game,
            recorder_mode: status.mode.as_str().to_string(),
            mic_enabled: status.mic,
        }
    }
}

/// What a clip trigger produced.
///
/// Not a [`ClipDto`]: a clip that was written but could not be indexed has no row, and reporting
/// it as a clip with a made-up id would hide the failure the index write has to be honest about.
/// `id` is the row when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedClipDto {
    /// The `clips` row, or `null` when the file was written but not indexed, in which case the UI
    /// must say so because the clip will not appear in the list.
    pub id: Option<i64>,
    pub path: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    /// The clip's first frame on the engine's media timeline.
    pub started_at_ms: u64,
}

/// The one recording this shell can have running, plus what a start is built from.
///
/// The engine is an `Arc<Recorder>` so a command can take a handle and drop the slot lock before
/// doing slow work (see `clip_now` above). `Mutex` because commands run on several threads.
pub struct RecorderHost {
    /// The directory `scratch/`, `clips/` and `localplay.db` resolve against, and where
    /// `config.toml` is read from when a recording starts.
    app_data_dir: PathBuf,
    /// ffmpeg, or `None` when it could not be found at startup, in which case recording is
    /// impossible and every start says so rather than failing somewhere deeper.
    bins: Option<FfmpegBinaries>,
    current: Mutex<Option<Arc<Recorder>>>,
    /// The game-event sources and the thread that turns their events into clips, alive for exactly
    /// as long as the recording they serve.
    events: Mutex<Option<EventDriver>>,
    /// Told about every clip that lands, whichever trigger took it (button, tray, hotkey or a
    /// game event). Set once by the shell, which owns the notification; empty in tests.
    clip_hook: Arc<OnceLock<ClipSavedHook>>,
}

/// What the shell does when a clip is saved (a toast and a sound, in the app).
pub type ClipSavedHook = Box<dyn Fn(&RecordedClipDto) + Send + Sync>;

impl RecordedClipDto {
    fn from_clip(clip: &RecordedClip) -> Self {
        Self {
            id: clip.id,
            path: clip.metadata.path.to_string_lossy().into_owned(),
            duration_ms: clip.metadata.duration_ms,
            size_bytes: clip.metadata.size_bytes,
            codec: clip.metadata.encoder.clone(),
            started_at_ms: clip.started_at_ms,
        }
    }
}

impl RecorderHost {
    pub fn new(app_data_dir: PathBuf, bins: Option<FfmpegBinaries>) -> Self {
        Self { app_data_dir, bins, current: Mutex::new(None),
            events: Mutex::new(None),
            clip_hook: Arc::new(OnceLock::new()),
        }
    }

    /// Register what happens when a clip is saved. Only the first call takes effect.
    pub fn on_clip_saved(&self, hook: impl Fn(&RecordedClipDto) + Send + Sync + 'static) {
        let _ = self.clip_hook.set(Box::new(hook));
    }

    fn announce(hook: &OnceLock<ClipSavedHook>, dto: &RecordedClipDto) {
        if let Some(hook) = hook.get() {
            hook(dto);
        }
    }

    /// The `config.toml` a recording would be started from.
    pub fn config_path(&self) -> PathBuf {
        self.app_data_dir.join("config.toml")
    }

    /// How each game-event source came out of the running recording's startup; empty when
    /// no recording (and so no driver) is up.
    pub fn event_reports(&self) -> Vec<localplay_recorder::event_driver::SourceReport> {
        self.events
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|d| d.reports().to_vec()))
            .unwrap_or_default()
    }

    /// The engine's own status, undigested (the health page wants the encoder name).
    pub fn raw_status(&self) -> RecorderStatus {
        self.handle().ok().flatten().map_or_else(RecorderStatus::stopped, |r| r.status())
    }

    /// The slot, or the error a poisoned slot deserves.
    ///
    /// Poisoning means a command panicked while holding the slot. Nothing here panics, so it would
    /// be a bug, and the honest answer is to refuse rather than pretend no recording is running.
    fn slot(&self) -> Result<MutexGuard<'_, Option<Arc<Recorder>>>, CommandError> {
        self.current.lock().map_err(|_| {
            CommandError::new(
                ErrorCode::Recording,
                "the recording state is unusable: an earlier command panicked while \
                 holding it",
            )
        })
    }

    /// A handle on the running recording, with the slot lock already released.
    fn handle(&self) -> Result<Option<Arc<Recorder>>, CommandError> {
        Ok(self.slot()?.as_ref().map(Arc::clone))
    }

    /// What the recorder is doing right now — never blocks the recording loop, and answers
    /// "not running" before anything has been started.
    pub fn status(&self) -> Result<RecordingStatusDto, CommandError> {
        Ok(match self.handle()? {
            Some(recorder) => RecordingStatusDto::from(recorder.status()),
            None => RecordingStatusDto::from(RecorderStatus::stopped()),
        })
    }

    /// Start recording, and return the first status.
    ///
    /// Everything the engine decides at startup (encoder smoke test, capture backend, ring, ffmpeg)
    /// happens here on the caller's thread, so a machine that cannot record says so now instead of
    /// failing inside a background thread.
    pub fn start(&self, settings: &RecordingConfig) -> Result<RecordingStatusDto, CommandError> {
        self.start_with(settings, Sources::Platform, false, true)
    }

    /// Start from the `config.toml` this host reads, as the window's Start button does.
    ///
    /// The tray's "Start recording" item and the hotkey's own recording both go through here, so
    /// there is one place that decides what a start means, including that the file is re-read at
    /// start time and not at window-open time (see `config.rs`).
    pub fn start_from_config(&self) -> Result<RecordingStatusDto, CommandError> {
        let settings = RecordingConfig::load(&self.config_path())?;
        self.start(&settings)
    }

    /// The body of a start. `sources` and `dev_software_encoder` are the engine's own choices,
    /// passed through unchanged: shipping code takes [`Self::start`], which is exactly
    /// [`Sources::Platform`] with the software encoder off.
    fn start_with(
        &self,
        settings: &RecordingConfig,
        sources: Sources,
        dev_software_encoder: bool,
        with_events: bool,
    ) -> Result<RecordingStatusDto, CommandError> {
        let bins = self.bins.as_ref().ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so a recording cannot be started. Install ffmpeg on \
                 PATH or place the sidecar binaries next to the application.",
            )
        })?;

        // The lock is held across the start so two concurrent starts cannot both win; a start is a
        // startup handshake (a one-frame smoke test at most), not a wait.
        let mut slot = self.slot()?;
        if slot.as_ref().is_some_and(|recorder| recorder.is_running()) {
            return Err(CommandError::new(
                ErrorCode::Recording,
                "a recording is already running: stop it before starting another",
            ));
        }

        let cfg = RecorderConfig {
            bin: bins.clone(),
            app_data_dir: self.app_data_dir.clone(),
            buffer: settings.buffer.clone(),
            encode: settings.encode.clone(),
            storage: settings.storage.clone(),
            capture: settings.capture.clone(),
            // The shell does not parse `[audio]` (see `config.rs`), so it carries neither the bitrate (the
            // encoder's own 192 kbps default applies) nor `enabled` (absent means game audio is recorded).
            // Setting either here would be a second source of truth for what the file owns, and this shell
            // has no switch for a video-only recording.
            audio_bitrate_kbps: None,
            audio_enabled: None,
            // WGC + WASAPI on Windows, synthetic stubs elsewhere. This shell has no switch for it and must
            // not: the engine refuses to substitute a stub on Windows, so no path here captures nothing
            // while looking like a recording.
            sources,
            dev_software_encoder,
        };

        // The file's [mic] and [games], not defaults: a start that ignored them would record without
        // the configured microphone and watcher. Malformed sections refuse the start, like the capture
        // sections: a typo'd mic flag must not record silently without it.
        let engine = EngineOptionsConfig::load(&self.config_path())?;
        let recorder = Recorder::start_with_options(cfg, recorder_options(&engine)).map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("could not start recording: {err:#}"))
        })?;
        let status = RecordingStatusDto::from(recorder.status());
        let recorder = Arc::new(recorder);
        *slot = Some(Arc::clone(&recorder));
        drop(slot);

        // Best effort, like everywhere the events are started: a taken port or a missing League install
        // is a warning, never a reason to refuse to record.
        if with_events {
            match EventDriver::spawn(
                recorder,
                &self.app_data_dir,
                &engine.events,
                Policy::new(&engine.events, settings.buffer.pre_seconds).map_err(|err| {
                    CommandError::new(ErrorCode::InvalidInput, format!("{err:#}"))
                })?,
                {
                    let hook = Arc::clone(&self.clip_hook);
                    Box::new(move |clip| Self::announce(&hook, &RecordedClipDto::from_clip(clip)))
                },
            ) {
                Ok(driver) => {
                    if let Ok(mut held) = self.events.lock() {
                        *held = Some(driver);
                    }
                }
                Err(err) => tracing::warn!("the game-event sources could not start: {err:#}"),
            }
        }
        Ok(status)
    }

    /// **TEST-ONLY.** Start with the engine's synthetic sources and its software encoder.
    ///
    /// The tray and the hotkey both end at [`Self::clip_now`], and the convincing test of that path
    /// is a real recording: a real encoder child, a real ring, a real splice and a real index row.
    /// [`Sources::Platform`] is a live Windows Graphics Capture session on Windows, which no test
    /// may open; [`Sources::Stub`] is the same engine over synthetic frames and is unreachable from
    /// a configuration file (see `localplay_recorder::Sources`). Gated on `cfg(test)`, so a shipped
    /// binary has no such entry point; the CLI's equivalent is `--dev-software-encoder`.
    #[cfg(test)]
    pub fn start_for_test_with_stub_sources(
        &self,
        settings: &RecordingConfig,
    ) -> Result<RecordingStatusDto, CommandError> {
        let stub = StubConfig { width: 1280, height: 720, fps: settings.encode.fps };
        self.start_with(settings, Sources::Stub(stub), true, false)
    }

    /// Stop the recording, flush the encoder and return the final status.
    ///
    /// Idempotent: with nothing running it reports the idle status rather than failing, so a stop
    /// from a window whose recording already died is not an error.
    pub fn stop(&self) -> Result<RecordingStatusDto, CommandError> {
        // Held across the stop: the join returns within one frame poll, and holding it makes "stop then
        // start" a sequence rather than a race between a closing and an opening capture session.
        // The sources go first: nothing should ask a recorder for a clip while it flushes.
        if let Ok(mut held) = self.events.lock() {
            held.take();
        }
        let taken = self.slot()?.take();
        let Some(recorder) = taken else {
            return Ok(RecordingStatusDto::from(RecorderStatus::stopped()));
        };
        let outcome = recorder.stop().map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("stopping the recorder failed: {err:#}"))
        });
        let status = RecordingStatusDto::from(recorder.status());
        // The status is reported either way (a recorder that failed while stopping still has counters
        // worth showing), but a failed stop is still a failure.
        outcome.map(|()| status)
    }

    /// Take a clip now.
    ///
    /// Blocks for the post-roll (seconds), so the caller must be off the UI thread: the footage is
    /// on disk before this returns.
    pub fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
        let recorder = self.handle()?.ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "nothing is recording, so there is no buffer to take a clip from",
            )
        })?;
        let clip = recorder.clip_now().map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("taking a clip failed: {err:#}"))
        })?;
        let dto = RecordedClipDto::from_clip(&clip);
        Self::announce(&self.clip_hook, &dto);
        Ok(dto)
    }

    /// Drop a marker on the running session's timeline (the bookmark hotkey). No clip is cut: it is
    /// a note recorded where a game event would be.
    pub fn bookmark(&self) -> Result<(), CommandError> {
        let recorder = self.handle()?.ok_or_else(|| {
            CommandError::new(ErrorCode::Recording, "nothing is recording, so there is no timeline to bookmark")
        })?;
        recorder
            .note_event(localplay_events::GameEvent::bare(
                localplay_events::Source::Manual,
                localplay_events::EventKind::Bookmark,
            ))
            .map(|_| ())
            .map_err(|err| {
                CommandError::new(ErrorCode::Recording, format!("the bookmark was not saved: {err:#}"))
            })
    }

    /// Whether a recording currently holds the slot (and the engine says it is running).
    pub fn is_running(&self) -> Result<bool, CommandError> {
        Ok(self.handle()?.is_some_and(|recorder| recorder.is_running()))
    }
}


#[cfg(test)]
mod tests {
    use localplay_media::probe::MediaInfo;
    use super::*;
    use crate::commands::test_support::*;
    use std::time::{Duration, Instant};

    // -- recording -----------------------------------------------------------------------

    /// A recorder host over the fixture's temporary directory.
    ///
    /// `with_ffmpeg` is false for the "no sidecars installed" case; when true the host holds the
    /// real binaries, but no test here lets a start reach capturing (see
    /// `a_start_the_engine_refuses_fails_before_any_capture_starts`).
    fn host(f: &Fixture, with_ffmpeg: bool) -> RecorderHost {
        let app_data_dir = f.paths.db_path.parent().expect("the db lives in the app dir");
        RecorderHost::new(app_data_dir.to_path_buf(), with_ffmpeg.then(|| f.bins.clone()))
    }

    #[test]
    fn recording_status_is_the_idle_status_before_anything_is_started() {
        let f = Fixture::new();
        let status = host(&f, true).status().unwrap();

        assert_eq!(status, RecordingStatusDto::from(RecorderStatus::stopped()));
        assert!(!status.running, "a shell that has never recorded reports not running");
        assert_eq!(status.configured_fps, 0, "and claims no configured rate");
        assert_eq!(status.effective_fps, 0, "nor a rate in use");
        assert_eq!(status.error, None);
    }

    #[test]
    fn a_start_without_ffmpeg_fails_with_the_code_the_ui_explains() {
        let f = Fixture::new();
        let host = host(&f, false);

        let err = host.start(&RecordingConfig::example().unwrap()).unwrap_err();

        assert_eq!(err.code, ErrorCode::FfmpegUnavailable);
        assert!(err.message.contains("ffmpeg was not found"), "got: {}", err.message);
        assert!(!host.status().unwrap().running, "and nothing was started");
    }

    #[test]
    fn a_start_the_engine_refuses_fails_before_any_capture_starts() {
        // The only test that reaches `RecorderHost::start` with real binaries: the engine resolves the
        // encoder (codec, then vendor) before it creates the capture backend, which on Windows is a
        // live Windows Graphics Capture session. This test fails the start at the codec on every
        // platform, so no capture session ever exists. A start that succeeded would record the screen
        // of whoever ran the suite.
        let f = Fixture::new();
        let host = host(&f, true);
        let mut settings = RecordingConfig::example().unwrap();
        settings.encode.codec = "vp9".to_string();

        let err = host.start(&settings).unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("unsupported encode.codec"),
            "the message must name the setting, through the command error: {}",
            err.message
        );
        assert!(!host.status().unwrap().running, "a failed start leaves nothing running");
    }

    /// **The integration test for the feature the background half exists for.**
    ///
    /// One press of the configured chord calls `RecorderHost::clip_now` (the call the window's Save
    /// clip button and the CLI's driver loop make), and that really produces a clip: a real encoder
    /// child, a real ring, a real lossless splice and a real index row.
    ///
    /// The trigger is an in-process function call. **No key, button or mouse event is synthesised,
    /// simulated or injected anywhere in this test or the code it drives**; only `RegisterHotKey`
    /// needs Windows. The capture source is the engine's synthetic stub, so the suite opens no
    /// capture session on the host's display, and the encoder is libx264 because a CI host has no
    /// GPU encoder.
    #[test]
    fn one_hotkey_press_writes_exactly_one_clip_through_the_recorder() {
        const PRE_SECONDS: u64 = 2;
        const POST_SECONDS: u64 = 1;

        let f = Fixture::new();
        let host = host(&f, true);

        // The example config shrunk to run in seconds: 2s pre-roll, 1s post-roll, 10fps, and the
        // capture backend's own size.
        let mut settings = RecordingConfig::example().unwrap();
        settings.buffer.pre_seconds = PRE_SECONDS;
        settings.buffer.post_seconds = POST_SECONDS;
        settings.buffer.segment_time = 1;
        settings.encode.fps = 10;
        settings.encode.output_size = String::new();
        settings.storage.max_total_bytes = u64::MAX;
        settings.storage.max_age_days = 3_650;

        let started = host.start_for_test_with_stub_sources(&settings).unwrap();
        assert!(started.running, "the engine is capturing before any press");

        // Wait for the ring to hold the whole window, as the CLI's self-test does before its trigger:
        // the engine refuses a trigger that asks for footage it does not have yet.
        let need_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = host.status().unwrap();
            if status.span_ms >= need_ms {
                break;
            }
            assert!(
                status.running,
                "the engine stopped while the ring was filling: {:?}",
                status.error
            );
            assert!(
                Instant::now() < deadline,
                "the ring never held {need_ms}ms of media (last: {}ms)",
                status.span_ms
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // THE PRESS. `on_hotkey_press` is what the hotkey thread calls; the closure is the call the
        // button makes.
        let outcome = crate::background::on_hotkey_press(|| host.clip_now());

        let status = host.status().unwrap();
        assert!(outcome.saved_a_clip(), "the press saved no clip: {outcome:?}");
        assert_eq!(status.clips, 1, "the engine counted exactly one clip, not two");

        // One file on disk, named as the trigger path names them.
        let clips: Vec<PathBuf> = std::fs::read_dir(&f.paths.clips_dir)
            .expect("the clips directory exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("clip-") && name.ends_with(".mp4"))
            })
            .collect();
        assert_eq!(clips.len(), 1, "expected exactly one clip file, got {clips:?}");

        // And it is real media of the configured window, produced by the real splice.
        //
        // The assertion is on the clip's video stream, not the container's duration: the splice
        // concatenates whole fragments on the encoder's grid, so the window quantises to that grid, and
        // the container also carries the audio track and stitched timestamps. Measured over fourteen
        // runs, plain and under load, the video stream is 3000ms (the window exactly) or 3100ms when the
        // window's edge falls inside a slightly longer fragment, while the container ran 3187-3396ms;
        // CI once reported 4393ms on a commit that changed no code. A container-duration assertion
        // measures the machine rather than the code.
        //
        // This pins that the video carries both sides of the window (a clip that stopped at the
        // pre-roll, ~2s, or was empty is the failure ruled out) and took the fragments around the
        // window rather than the whole ring, which is the overhang bound below. That bound is one
        // fragment at each end; a fragment can exceed `segment_time` when the host starves the encoder,
        // so it is not tighter.
        let info = MediaInfo::probe(&f.bins, &clips[0]).expect("the clip is readable media");
        let expected_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        let segment_ms = settings.buffer.segment_time * 1_000;
        let video = info.video.as_ref().expect("a video stream");
        let video_ms = video.duration_ms.expect("ffprobe reports the video stream's duration");
        assert!(
            video_ms >= expected_ms * 3 / 4 && video_ms <= expected_ms + 2 * segment_ms,
            "the clip's video is {video_ms}ms, which is not the configured window \
             ({expected_ms}ms ± the segment grid): a clip that carries only one side of the \
             window, or is empty, is the failure this rules out"
        );
        assert_eq!(video.codec, "h264", "the software encoder is libx264 in a test build");

        // One row in the index: the row `clip_now` wrote, in the store this shell reads.
        let rows = f.store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the clip was indexed exactly once");
        assert_eq!(rows[0].path, clips[0], "and the row names the file on disk");

        // A stop flushes the encoder and closes the capture session.
        let stopped = host.stop().unwrap();
        assert!(!stopped.running);
    }

    #[test]
    fn stopping_nothing_is_idempotent_and_says_so() {
        let f = Fixture::new();
        let host = host(&f, true);

        let first = host.stop().unwrap();
        let second = host.stop().unwrap();

        assert_eq!(first, second, "a stop with nothing running is a value, not a failure");
        assert!(!first.running);
        assert_eq!(first.error, None);
    }

    #[test]
    fn a_clip_with_nothing_recording_is_refused_rather_than_waited_for() {
        let f = Fixture::new();
        let host = host(&f, true);

        let err = host.clip_now().unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("nothing is recording"),
            "the message must say why there is no clip: {}",
            err.message
        );
    }

    #[test]
    fn the_recording_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these two structs by hand, like the clip and storage DTOs: without
        // this, a rename on either side is found by a user watching a status readout that never moves.
        let status = serde_json::to_value(RecordingStatusDto::from(RecorderStatus::stopped())).unwrap();
        let mut keys: Vec<&str> = status.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes",
                "clips",
                "configured_fps",
                "drift_ms",
                "dropped",
                "dropped_audio",
                "effective_fps",
                "error",
                "fps",
                "frames",
                "matched_game",
                "mic_enabled",
                "recorder_mode",
                "running",
                "segments",
                "skipped",
                "span_ms",
                "watching_games"
            ]
        );

        let clip = serde_json::to_value(RecordedClipDto {
            id: Some(3),
            path: "/clips/clip-1.mp4".to_string(),
            duration_ms: 12_000,
            size_bytes: 1_024,
            codec: "libx264".to_string(),
            started_at_ms: 4_000,
        })
        .unwrap();
        let mut keys: Vec<&str> = clip.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["codec", "duration_ms", "id", "path", "size_bytes", "started_at_ms"]
        );
        assert_eq!(clip["id"], 3, "an indexed clip carries its row id");
    }

    #[test]
    fn the_status_dto_carries_the_watcher_the_game_and_the_microphone() {
        // The engine tracks all four; the window needs them to say whether anything is being watched.
        let status = RecorderStatus {
            watching_games: true,
            game: Some("Dota 2".to_string()),
            mode: localplay_recorder::RecordingMode::FullSession,
            mic: true,
            ..RecorderStatus::stopped()
        };

        let dto = RecordingStatusDto::from(status);

        assert!(dto.watching_games);
        assert_eq!(dto.matched_game.as_deref(), Some("Dota 2"));
        assert_eq!(dto.recorder_mode, "session");
        assert!(dto.mic_enabled);

        let idle = RecordingStatusDto::from(RecorderStatus::stopped());
        assert!(!idle.watching_games);
        assert_eq!(idle.matched_game, None);
        assert_eq!(idle.recorder_mode, "buffer");
        assert!(!idle.mic_enabled);
    }
}
