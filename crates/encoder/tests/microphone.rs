//! Two audio tracks in one MP4: the game/system audio and the microphone.
//!
//! The feature is "one more input", and what it has to get right is exactly that: the second
//! audio input is *declared* (a second loopback listener, `-map 2:a`, its own format), it is
//! *tagged* so a player can tell the tracks apart, and it carries **its own** audio — a
//! mixed-up mapping, or a second input accidentally folded into the first, would produce a
//! file that still has two audio streams and is still wrong.
//!
//! So the two sources feed deliberately different, far-apart tones (200 Hz for the game
//! audio, 6 kHz for the microphone) and each resulting track is measured in the *other*
//! tone's band. A track that carries its own tone and not the other's is the proof the two
//! inputs stayed separate; the streams and their tags come from ffprobe's own JSON, read by
//! this file's scanner for the second audio stream and by the workspace's `MediaInfo` for
//! everything it already knows how to read.
//!
//! Fed in real time, like every other end-to-end test here: the video input is timestamped
//! from arrival (`-use_wallclock_as_timestamps 1`) and the audio timeline is the sample
//! count, so a burst would encode correctly as a few milliseconds of media and there would
//! be nothing left to measure.

use localplay_capture::stub::{StubCapture, StubConfig};
use localplay_capture::{AudioBuffer, AudioFormat, CaptureBackend};
use localplay_encoder::{
    EncodeConfig, Encoder, FfmpegEncoder, MicAudioSpec, SampleFormat, VideoCodec,
};
use localplay_media::{FfmpegBinaries, MediaInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The game audio's tone and the microphone's. Five octaves apart on purpose: see
/// [`band_energy_db`], which has to be able to tell them apart by tens of dB.
const GAME_TONE_HZ: f64 = 200.0;
const MIC_TONE_HZ: f64 = 6_000.0;

/// The width of the band measured around a tone, and how far the *other* tone's energy has
/// to be below this track's own for the track to count as carrying this tone.
///
/// 20 dB is far above the AAC noise floor in a 200 Hz band and far below the separation a
/// five-octave gap gives, so it is a threshold no honest signal can drift across.
const MEASURE_BAND_HZ: u32 = 200;
const SEPARATION_DB: f64 = 20.0;

/// Below this a track counts as silent, which is the failure the two tones are there to
/// catch in the first place (an unmapped or unmixed-up-but-empty track).
const AUDIBLE_DB: f64 = -40.0;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 30;
const SEGMENT_MS: u64 = 1_000;
/// How long the pipeline is driven for, in wall-clock seconds.
const FEED: Duration = Duration::from_secs(3);
/// The single-track test needs no tone measurement, so it only has to produce a segment.
const FEED_WITHOUT_MIC: Duration = Duration::from_millis(1_500);

/// Both tests here drive a real encoder against the wall clock, so they take turns rather
/// than measuring each other's CPU contention (the same reasoning as `timeline.rs`).
fn wall_clock_slot() -> MutexGuard<'static, ()> {
    static SLOT: OnceLock<Mutex<()>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A phase-continuous sine, delivered in the 10 ms blocks the capture backends publish.
struct Tone {
    hz: f64,
    phase: f64,
    pts: Duration,
}

impl Tone {
    fn new(hz: f64) -> Self {
        Self { hz, phase: 0.0, pts: Duration::ZERO }
    }

    /// 480 frames of 48 kHz stereo s16le — one 10 ms block, like `StubAudio`'s.
    fn block(&mut self) -> AudioBuffer {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 480;
        let mut data = Vec::with_capacity(FRAMES * 2 * 2);
        for _ in 0..FRAMES {
            let sample = (0.5 * (std::f64::consts::TAU * self.hz * self.phase / RATE).sin()
                * f64::from(i16::MAX)) as i16;
            self.phase += 1.0;
            data.extend_from_slice(&sample.to_le_bytes());
            data.extend_from_slice(&sample.to_le_bytes());
        }
        let block =
            AudioBuffer { data, frames: FRAMES, pts: self.pts, format: AudioFormat::default() };
        self.pts += Duration::from_millis(10);
        block
    }
}

/// Drive the encoder for `feed` of wall clock: video from the capture stub, and one 10 ms
/// audio block per source per 10 ms of real time — the schedule both audio backends publish
/// on. Returns the microseconds of audio fed per track (they are pushed in lockstep).
fn feed_for(
    encoder: &mut FfmpegEncoder,
    video: &mut StubCapture,
    game: &mut Tone,
    mut mic: Option<&mut Tone>,
    feed: Duration,
) {
    let started = Instant::now();
    let mut next_block_at = started;
    while started.elapsed() < feed {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            encoder.submit_video(frame).expect("submit video");
        }
        let now = Instant::now();
        while next_block_at <= now {
            encoder.submit_audio(game.block()).expect("submit game audio");
            if let Some(tone) = mic.as_deref_mut() {
                encoder.submit_mic_audio(tone.block()).expect("submit microphone audio");
            }
            next_block_at += Duration::from_millis(10);
        }
    }
}

/// The microphone's MP4: two audio tracks, tagged, each carrying its own tone, and no
/// disturbance to the video timeline.
#[test]
fn the_microphone_lands_as_a_second_tagged_track_carrying_its_own_tone() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        FPS,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    // The opt-in, as a caller would set it from config.toml.
    cfg.mic_audio = Some(MicAudioSpec::default());

    let mut encoder =
        FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder with a microphone");
    assert!(
        encoder.mic_port().is_some(),
        "the encoder owns the microphone listener, and reports its port"
    );

    let mut video = StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS });
    video.start().expect("start the capture stub");
    let mut game = Tone::new(GAME_TONE_HZ);
    let mut mic = Tone::new(MIC_TONE_HZ);
    feed_for(&mut encoder, &mut video, &mut game, Some(&mut mic), FEED);
    encoder.finish().expect("flush encoder");

    assert_eq!(
        encoder.dropped_mic_audio_blocks(),
        0,
        "no microphone block may be lost: the track's timeline is its sample count"
    );
    assert_eq!(encoder.dropped_audio_blocks(), 0, "and neither may the game audio's");

    let segments = segments_in(dir.path());
    assert!(segments.len() >= 2, "3s of wall clock at 1s segments: got {}", segments.len());
    let first = &segments[0];

    // ---- what the container says ----------------------------------------------------
    let json = probe_json(&bin, first);
    let streams = streams_of(&json);
    let video_streams: Vec<&ProbedStream> =
        streams.iter().filter(|s| s.codec_type == "video").collect();
    let audio_streams: Vec<&ProbedStream> =
        streams.iter().filter(|s| s.codec_type == "audio").collect();

    assert_eq!(video_streams.len(), 1, "one video stream: {streams:#?}");
    assert_eq!(
        audio_streams.len(),
        2,
        "the microphone is a SECOND audio track, not a replacement and not a second file: \
         {streams:#?}"
    );
    assert_eq!(
        audio_streams[0].title.as_deref(),
        Some("Game Audio"),
        "the first audio track keeps its tag (`title=` metadata, stored by the MP4 muxer in \
         the track's `name` atom — see `track_title`): {streams:#?}"
    );
    assert_eq!(
        audio_streams[1].title.as_deref(),
        Some("Microphone"),
        "and the second one is named: {streams:#?}"
    );

    // The workspace's own probe reads the same document — the two views have to agree about
    // the file, not merely both exist.
    let info = MediaInfo::from_ffprobe_json(&json).expect("the media crate parses this probe");
    assert!(info.video.is_some(), "a video stream is still there");
    let first_audio = info.audio.expect("an audio stream is still there");
    assert_eq!(first_audio.codec, "aac");
    assert_eq!(first_audio.sample_rate, 48_000);
    assert_eq!(first_audio.channels, 2);

    // ---- what each track actually carries -------------------------------------------
    let game_own = band_energy_db(&bin, first, 0, GAME_TONE_HZ);
    let game_other = band_energy_db(&bin, first, 0, MIC_TONE_HZ);
    let mic_own = band_energy_db(&bin, first, 1, MIC_TONE_HZ);
    let mic_other = band_energy_db(&bin, first, 1, GAME_TONE_HZ);
    eprintln!(
        "track 1 (Game Audio): {game_own:.1} dB at {GAME_TONE_HZ} Hz, {game_other:.1} dB at \
         {MIC_TONE_HZ} Hz; track 2 (Microphone): {mic_own:.1} dB at {MIC_TONE_HZ} Hz, \
         {mic_other:.1} dB at {GAME_TONE_HZ} Hz"
    );

    assert!(game_own > AUDIBLE_DB, "the game track is not silent: {game_own:.1} dB");
    assert!(mic_own > AUDIBLE_DB, "the microphone track is not silent: {mic_own:.1} dB");
    assert!(
        game_own - game_other > SEPARATION_DB,
        "track 1 must carry the game tone and not the microphone's: {game_own:.1} dB in its \
         own band against {game_other:.1} dB in the other. Two tracks that both contain both \
         tones would mean the inputs were mixed into one stream"
    );
    assert!(
        mic_own - mic_other > SEPARATION_DB,
        "track 2 must carry the microphone tone and not the game's: {mic_own:.1} dB in its \
         own band against {mic_other:.1} dB in the other"
    );

    // ---- and how the three streams sit against each other ---------------------------
    let mut video_ms = 0u64;
    let mut game_ms = 0u64;
    let mut mic_ms = 0u64;
    for path in &segments {
        let streams = streams_of(&probe_json(&bin, path));
        video_ms += duration_of(&streams, "video", 0);
        game_ms += audio_duration_of(&streams, 0);
        mic_ms += audio_duration_of(&streams, 1);
    }
    eprintln!(
        "{} segments over {FEED:?}: video {video_ms}ms, game audio {game_ms}ms, microphone \
         {mic_ms}ms (the two audio tracks were fed the same blocks on the same schedule)",
        segments.len()
    );

    // The two audio tracks are fed identically, so their media clocks must agree to within
    // an AAC frame; a gap here would mean one track lost blocks.
    assert!(
        game_ms.abs_diff(mic_ms) <= 120,
        "the game and microphone tracks must cover the same time: {game_ms}ms against \
         {mic_ms}ms"
    );
    // The video timeline is the arrival clock (see the module comment), so it has to cover
    // the feed as well — and a second audio input must not have disturbed it.
    assert!(
        video_ms.abs_diff(game_ms) <= 500,
        "3s of capture must still encode as ~3s of video alongside two audio tracks: \
         video {video_ms}ms against audio {game_ms}ms"
    );
    assert!(
        (2_500..=3_600).contains(&video_ms),
        "the video timeline is expected to cover the feed: {video_ms}ms for {FEED:?}"
    );
}

/// The pre-existing shape, still: no microphone, one audio track, exactly as before.
///
/// This is the end-to-end half of the byte-identical contract (the other half is the
/// argument-list test in `src/ffmpeg.rs`): with `mic_audio: None` the file a segment muxer
/// writes carries one video stream and ONE audio stream, untagged, and the encoder reports
/// no microphone port.
#[test]
fn without_a_microphone_the_segment_still_carries_exactly_one_audio_track() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        FPS,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    assert!(cfg.mic_audio.is_none(), "the microphone is opt-in");

    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");
    assert_eq!(encoder.mic_port(), None, "no microphone, no microphone port");

    let mut video = StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS });
    video.start().expect("start the capture stub");
    let mut game = Tone::new(GAME_TONE_HZ);
    feed_for(&mut encoder, &mut video, &mut game, None, FEED_WITHOUT_MIC);
    encoder.finish().expect("flush encoder");

    let segments = segments_in(dir.path());
    assert!(!segments.is_empty(), "a segment was written");
    let streams = streams_of(&probe_json(&bin, &segments[0]));

    assert_eq!(streams.iter().filter(|s| s.codec_type == "video").count(), 1, "{streams:#?}");
    assert_eq!(
        streams.iter().filter(|s| s.codec_type == "audio").count(),
        1,
        "an encoder spawned without a microphone must produce the single-track output it \
         always did — no second input, no mapping, no tags: {streams:#?}"
    );
    // No mapping means no title metadata at all: the one audio stream is untagged, exactly
    // as it was before the microphone input existed.
    assert_eq!(
        streams.iter().find(|s| s.codec_type == "audio").and_then(|s| s.title.clone()),
        None,
        "the single-track path gains no metadata: {streams:#?}"
    );
}

/// Every way a microphone block can be refused, refused **out loud**.
///
/// The one outcome that must not exist anywhere on this path is "drop the block and carry
/// on": that leaves a recording whose container says two tracks and whose second track is
/// empty or shortened, and nothing downstream can tell. So both refusals are errors that
/// name themselves — one when the encoder has no second input at all, one when the block's
/// format contradicts what the child was told (which would put the track on a timeline of
/// the wrong length: 48kHz PCM read under a 44.1kHz declaration plays 8.8% fast and slides
/// against the picture for the whole clip).
#[test]
fn a_microphone_block_the_encoder_cannot_use_is_refused_out_loud() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

    // 1. Spawned WITHOUT a microphone: there is no second input to hand a block to.
    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        FPS,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    let mut without = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");
    let err = without
        .submit_mic_audio(Tone::new(MIC_TONE_HZ).block())
        .expect_err("an encoder with no microphone input cannot accept a microphone block");
    assert!(
        err.to_string().contains("mic_audio was None"),
        "the refusal names the reason, so a caller can tell it apart from a full queue: {err}"
    );
    drop(without);

    // 2. Spawned WITH a microphone, but the block's own format is not what was declared.
    let mut cfg = cfg;
    cfg.mic_audio =
        Some(MicAudioSpec { sample_rate: 44_100, channels: 1, sample_format: SampleFormat::S16Le });
    let mut declared_44k_mono = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");
    let err = declared_44k_mono
        .submit_mic_audio(Tone::new(MIC_TONE_HZ).block()) // 48kHz stereo, like every backend here
        .expect_err("a block that contradicts the declaration must be refused, not queued");
    let message = err.to_string();
    assert!(
        message.contains("48000") && message.contains("44100"),
        "the refusal has to name both formats — the block's and the declaration's: {message}"
    );
}

/// The scratch directory's segments, in ffmpeg's own order.
fn segments_in(dir: &Path) -> Vec<PathBuf> {
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read the scratch dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    segments
}

/// `ffprobe`'s JSON for one file, so the scanner below and `MediaInfo` read the same bytes.
fn probe_json(bin: &FfmpegBinaries, path: &Path) -> String {
    let out = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_streams", "-show_format"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One stream of the produced file, as far as this file needs it.
#[derive(Debug, Clone)]
struct ProbedStream {
    codec_type: String,
    /// The stream's title, which is how the two audio tracks are told apart. See
    /// [`track_title`] for why this is not simply the JSON key `title`.
    title: Option<String>,
    /// `stream.duration`, in whole milliseconds, when ffprobe reports one.
    duration_ms: Option<u64>,
}

fn duration_of(streams: &[ProbedStream], codec_type: &str, index: usize) -> u64 {
    streams
        .iter()
        .filter(|s| s.codec_type == codec_type)
        .nth(index)
        .and_then(|s| s.duration_ms)
        .unwrap_or_else(|| panic!("no duration for {codec_type} #{index} in {streams:#?}"))
}

fn audio_duration_of(streams: &[ProbedStream], index: usize) -> u64 {
    duration_of(streams, "audio", index)
}

/// The `streams` array of an ffprobe document, one entry per stream.
///
/// A deliberately small scanner rather than a JSON dependency: the workspace's `MediaInfo`
/// keeps only the **first** audio stream (that is the media crate's contract, and the
/// microphone is the second one), and it does not carry tags at all, which are the whole
/// point of `-metadata:s:a:0 title=Game Audio`. The parser has to see every stream and its
/// title, so it reads exactly the slice of JSON ffprobe emits here — objects of scalars and
/// one nested `tags` object — and panics rather than guessing when the shape is not there.
fn streams_of(json: &str) -> Vec<ProbedStream> {
    let streams_at = json.find("\"streams\"").expect("ffprobe json has a streams array");
    let open = streams_at + json[streams_at..].find('[').expect("streams is an array");
    let close = matching_delimiter(json, open, '[', ']');
    let body = &json[open + 1..close];

    let mut objects = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find('{') {
        let start = cursor + rel;
        let end = matching_delimiter(body, start, '{', '}');
        objects.push(&body[start..=end]);
        cursor = end + 1;
    }

    objects
        .into_iter()
        .map(|object| ProbedStream {
            codec_type: string_field(object, "codec_type").unwrap_or_default(),
            title: track_title(object),
            duration_ms: string_field(object, "duration")
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0).round() as u64),
        })
        .collect()
}

/// The byte offset of the delimiter matching the one at `at`, string-aware.
fn matching_delimiter(s: &str, at: usize, open: char, close: char) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in s[at..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return at + i;
            }
        }
    }
    panic!("no matching {close} for the {open} at byte {at}");
}

/// The title of a stream, as the container actually carries it.
///
/// The encoder is *told* `-metadata:s:a:0 title=Game Audio`, and ffmpeg's MP4 muxer stores a
/// stream's `title` metadata in the track's `name` atom — which is what ffprobe reports back
/// and what a player shows as the track's name. Measured here, on a segment written by this
/// very argument list:
///
/// ```text
/// "tags": { "language": "und", "handler_name": "SoundHandler", "name": "Game Audio" }
/// ```
///
/// So `name` is read first and `title` is accepted as well, so that a container that spells
/// the tag its own way is reported for what it is rather than as "no tag at all". The
/// needles carry their quotes, so `codec_name` and `handler_name` cannot match either one.
fn track_title(object: &str) -> Option<String> {
    string_field(object, "name").or_else(|| string_field(object, "title"))
}

/// The value of a `"key": "value"` pair inside one stream object, if present.
///
/// `title` lives under `tags` rather than directly on the stream, but the objects ffprobe
/// emits for a stream have no other field of that name, and the keys searched for here
/// (`codec_type`, `name`, `title`, `duration`) are unambiguous within one object.
fn string_field(object: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = object.find(&needle)? + needle.len();
    let rest = object[after_key..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The mean volume of one audio stream inside a narrow band around `hz`, in dB.
///
/// `volumedetect` prints the mean level of what it is handed; behind a narrow `bandpass`
/// that number is the energy this track carries *at that frequency*, which is what tells the
/// two tones apart. A track that has no tone at `hz` reads tens of dB lower than one that
/// has it, and a track that is silent reads `-inf`.
fn band_energy_db(bin: &FfmpegBinaries, path: &Path, stream: usize, hz: f64) -> f64 {
    let filter = format!("bandpass=f={hz}:width_type=h:w={MEASURE_BAND_HZ},volumedetect");
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{stream}"), "-af", &filter, "-f", "null", "-"])
        .output()
        .expect("run ffmpeg to measure a band");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "measuring a:{stream} at {hz}Hz on {} failed: {stderr}",
        path.display()
    );
    mean_volume_db(&stderr)
        .unwrap_or_else(|| panic!("no `mean_volume` in ffmpeg's output: {stderr}"))
}

/// `[volumedetect] mean_volume: -23.4 dB`, or `-inf dB` for silence.
fn mean_volume_db(stderr: &str) -> Option<f64> {
    let line = stderr.lines().find(|line| line.contains("mean_volume:"))?;
    let value = line.split("mean_volume:").nth(1)?.trim().strip_suffix("dB")?.trim();
    if value == "-inf" {
        return Some(f64::NEG_INFINITY);
    }
    value.parse().ok()
}
