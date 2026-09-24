//! The hotkey path's post-splice step, end to end on this host: a real clip spliced by
//! ffmpeg → a real `clips` row in a real index → the storage policy managing that clip.
//!
//! The bug this file exists to prevent is the one the store crate was written for and the
//! application then never called: clips were written to a directory and never indexed, so
//! nothing could list them and nothing would ever delete them.
//!
//! Gating and reach: the global hotkey cannot fire off Windows (`localplay_events::hotkey`
//! installs a real `RegisterHotKey` message loop only there), so the CLI's capture loop
//! itself cannot be driven from a test on this host. Everything the hotkey *branch* does
//! after its post-roll wait can be, and is, driven here with the same calls the engine
//! makes: the in-memory ring's `trigger` to splice, `localplay_recorder::index_clip` to index, and
//! the policy functions to evict. (Those calls are the recorder crate's rather than this
//! library's since the pipeline moved there; what this file still pins is that the whole
//! sequence — splice, index, plan, evict — works over real ffmpeg output.)
//!
//! Like the rest of this package's tests, it needs ffmpeg on `PATH` with a working
//! libx264 (see `post_roll.rs` on why the test-encoders feature is guaranteed by the
//! dev-dependency rather than by a `cfg` gate).

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_recorder::memory_ring::{MemoryRing, RingSetup};
use localplay_recorder::{pump_until_span_on, FramePacer};
use localplay_encoder::{EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::FfmpegBinaries;
use localplay_store::cleanup::{execute_cleanup, plan_cleanup, CleanupPolicy};
use localplay_store::Store;
use std::time::Duration;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 10;
const SEGMENT_MS: u64 = 1_000;
const PRE_MS: u64 = 2_000;
const POST_MS: u64 = 1_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[test]
fn a_spliced_clip_is_indexed_with_its_real_values_and_the_policy_can_evict_it() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    // The buffer keeps its footage in RAM, so a segment directory is only needed because the
    // test encoder's constructor takes one; nothing is written into it in stream mode.
    let segment_dir = tempfile::tempdir().expect("a segment dir");
    let clips = tempfile::tempdir().expect("a clips dir");

    let mut encode = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        FPS,
        segment_dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    encode.output = EncodeOutput::FragmentedStream;

    let mut capture = StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS });
    let mut audio = StubAudio::new(AudioFormat::default());
    let mut encoder = FfmpegEncoder::spawn(&bin, &encode).expect("spawn the encoder");
    let stream = encoder.take_output_stream().expect("the stream mode exposes its pipe");
    let mut ring = MemoryRing::start(
        stream,
        RingSetup {
            bin: bin.clone(),
            clips_dir: clips.path().to_path_buf(),
            ram_cap_bytes: 256 * 1024 * 1024,
            cap_ms: PRE_MS + POST_MS + SEGMENT_MS,
            pre_ms: PRE_MS,
            post_ms: POST_MS,
            encoder: "libx264".to_string(),
        },
    )
    .expect("start the in-memory ring");

    // The pipeline runs in real time: the encoder's media timeline is the wall clock, so
    // this covers `need_ms` of footage after roughly `need_ms` of wall clock.
    capture.start().expect("start the stub capture");
    audio.start().expect("start the stub audio");
    let mut pacer = FramePacer::new(FPS);
    let trigger_ms = PRE_MS; // run-relative media time, exactly as the CLI takes it
    let need_ms = trigger_ms + POST_MS;
    pump_until_span_on(
        &mut pacer,
        &mut ring,
        &mut capture,
        &mut audio,
        None,
        &mut encoder,
        need_ms,
        Duration::from_secs(60),
    )
    .expect("the post-roll must be reachable");

    // Step 5 of spec §6.2, the same call the hotkey branch makes.
    let clip = ring.trigger(trigger_ms, "indexed-clip").expect("splice the clip");
    assert!(clip.path.is_file(), "the clip must exist on disk");
    assert_eq!(clip.encoder, "libx264");

    // Step 6, and the code under test: the clip becomes a row.
    let db_dir = tempfile::tempdir().expect("a temp dir for the index");
    let store = Store::open(&db_dir.path().join("localplay.db")).expect("open the clip index");
    store.migrate().expect("migrate the clip index");

    // The window's start on the ring's own clock. An in-memory ring starts at zero every run,
    // so unlike the file ledger's there is no origin to add: the trigger instant and the
    // footage are already on the same timeline.
    let started_at_ms = trigger_ms.saturating_sub(PRE_MS);
    let id = localplay_recorder::index_clip(&store, &clip, started_at_ms).expect("the clip is indexed");

    let rows = store.list_clips().expect("list the indexed clips");
    assert_eq!(rows.len(), 1, "one clip, one row");
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.path, clip.path, "the row names the file that was written");
    assert_eq!(
        row.size_bytes,
        std::fs::metadata(&clip.path).expect("stat the clip").len(),
        "the indexed size is the size of the real file"
    );
    assert_eq!(row.size_bytes, clip.size_bytes);
    assert_eq!(row.duration_ms, clip.duration_ms);
    assert!(row.duration_ms > 0, "the clip has a duration");
    assert_eq!(row.codec, "libx264", "the encoder that actually produced the file");
    assert_eq!(row.started_at_ms, started_at_ms);
    assert!(!row.favourite);
    assert!(row.created_at_ms > 0, "the wall-clock creation time was stamped");
    assert_eq!(store.total_bytes().expect("sum the index"), row.size_bytes);
    eprintln!(
        "indexed clip #{id}: {} (media t={}ms, {}ms, {} bytes, {})",
        row.path.display(),
        row.started_at_ms,
        row.duration_ms,
        row.size_bytes,
        row.codec
    );

    // Now the storage policy: a zero cap makes every non-favourited clip evictable, and
    // the clip that was just indexed is the only one there is.
    let policy = CleanupPolicy { max_total_bytes: 0, max_age_days: 3_650 };
    let plan = plan_cleanup(&store.list_clips().expect("list"), &policy, now_ms());
    assert_eq!(plan.ids(), vec![id], "the policy selects the clip the hotkey produced");
    let outcome = execute_cleanup(&store, &plan).expect("execute the plan");
    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.bytes_reclaimed, clip.size_bytes);
    assert!(outcome.cap_met, "with no favourites left, the cap is met");
    assert_eq!(outcome.bytes_after, 0);
    assert!(store.list_clips().expect("list").is_empty(), "the row is gone");
    assert!(
        !clip.path.exists(),
        "and the file went with it — unlinked only after the row delete committed"
    );

    encoder.finish().expect("flush the encoder");
}
