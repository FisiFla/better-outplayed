//! Two audio tracks in one MP4: the game/system audio and the microphone.
//!
//! The risk is a second input that is declared but mixed up: a wrong `-map` or a folded-in input
//! still yields two audio streams and is still wrong. So the sources feed far-apart tones (200 Hz
//! game, 6 kHz mic) and each track is measured in the other tone's band. Streams and tags come
//! from ffprobe's JSON, read by this file's scanner (the media crate's `MediaInfo` keeps only the
//! first audio stream and no tags).
//!
//! Fed in real time like every other end-to-end test: video is timestamped from arrival and audio
//! from the sample count, so a burst would encode as a few milliseconds of media.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
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

/// The game and microphone tones, five octaves apart so [`band_energy_db`] separates them by tens of dB.
const GAME_TONE_HZ: f64 = 200.0;
const MIC_TONE_HZ: f64 = 6_000.0;

/// Width of the band measured around a tone. The other tone must sit 20 dB below the track's own,
/// far above the AAC noise floor and far below what a five-octave gap gives.
const MEASURE_BAND_HZ: u32 = 200;
const SEPARATION_DB: f64 = 20.0;

/// Below this a track counts as silent (an unmapped or empty track).
const AUDIBLE_DB: f64 = -40.0;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 30;
const SEGMENT_MS: u64 = 1_000;
/// Wall-clock seconds the pipeline is driven for.
const FEED: Duration = Duration::from_secs(3);
/// The single-track test needs no tone measurement, only a segment.
const FEED_WITHOUT_MIC: Duration = Duration::from_millis(1_500);

/// Both tests drive a real encoder against the wall clock, so they take turns (as in `timeline.rs`).
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
        Self {
            hz,
            phase: 0.0,
            pts: Duration::ZERO,
        }
    }

    /// One 10 ms block: 480 frames of 48 kHz stereo s16le.
    fn block(&mut self) -> AudioBuffer {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 480;
        let mut data = Vec::with_capacity(FRAMES * 2 * 2);
        for _ in 0..FRAMES {
            let sample = (0.5
                * (std::f64::consts::TAU * self.hz * self.phase / RATE).sin()
                * f64::from(i16::MAX)) as i16;
            self.phase += 1.0;
            data.extend_from_slice(&sample.to_le_bytes());
            data.extend_from_slice(&sample.to_le_bytes());
        }
        let block = AudioBuffer {
            data,
            frames: FRAMES,
            pts: self.pts,
            format: AudioFormat::default(),
        };
        self.pts += Duration::from_millis(10);
        block
    }
}

/// Drive the encoder for `feed` of wall clock: video from the capture stub plus one 10 ms audio
/// block per source per 10 ms. Returns the microseconds of audio fed per track (pushed in lockstep).
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
        if let Some(frame) = video
            .next_frame(Duration::from_millis(5))
            .expect("next frame")
        {
            encoder.submit_video(frame).expect("submit video");
        }
        let now = Instant::now();
        while next_block_at <= now {
            encoder
                .submit_audio(game.block())
                .expect("submit game audio");
            if let Some(tone) = mic.as_deref_mut() {
                encoder
                    .submit_mic_audio(tone.block())
                    .expect("submit microphone audio");
            }
            next_block_at += Duration::from_millis(10);
        }
    }
}

/// Two tagged audio tracks, each carrying its own tone, with the video timeline undisturbed.
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
    // The opt-in, as config.toml sets it.
    cfg.mic_audio = Some(MicAudioSpec::default());

    let mut encoder =
        FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder with a microphone");
    assert!(
        encoder.mic_port().is_some(),
        "the encoder owns the microphone listener, and reports its port"
    );

    let mut video = StubCapture::new(StubConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: FPS,
    });
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
    assert_eq!(
        encoder.dropped_audio_blocks(),
        0,
        "and neither may the game audio's"
    );

    let segments = segments_in(dir.path());
    assert!(
        segments.len() >= 2,
        "3s of wall clock at 1s segments: got {}",
        segments.len()
    );
    let first = &segments[0];

    // What the container says.
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

    // The media crate's probe must agree with the scanner about the file.
    let info = MediaInfo::from_ffprobe_json(&json).expect("the media crate parses this probe");
    assert!(info.video.is_some(), "a video stream is still there");
    let first_audio = info.audio.expect("an audio stream is still there");
    assert_eq!(first_audio.codec, "aac");
    assert_eq!(first_audio.sample_rate, 48_000);
    assert_eq!(first_audio.channels, 2);

    // What each track carries.
    let game_own = band_energy_db(&bin, first, 0, GAME_TONE_HZ);
    let game_other = band_energy_db(&bin, first, 0, MIC_TONE_HZ);
    let mic_own = band_energy_db(&bin, first, 1, MIC_TONE_HZ);
    let mic_other = band_energy_db(&bin, first, 1, GAME_TONE_HZ);
    eprintln!(
        "track 1 (Game Audio): {game_own:.1} dB at {GAME_TONE_HZ} Hz, {game_other:.1} dB at \
         {MIC_TONE_HZ} Hz; track 2 (Microphone): {mic_own:.1} dB at {MIC_TONE_HZ} Hz, \
         {mic_other:.1} dB at {GAME_TONE_HZ} Hz"
    );

    assert!(
        game_own > AUDIBLE_DB,
        "the game track is not silent: {game_own:.1} dB"
    );
    assert!(
        mic_own > AUDIBLE_DB,
        "the microphone track is not silent: {mic_own:.1} dB"
    );
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

    // How the three streams sit against each other.
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

    // Both audio tracks are fed identically, so their clocks agree to within an AAC frame.
    assert!(
        game_ms.abs_diff(mic_ms) <= 120,
        "the game and microphone tracks must cover the same time: {game_ms}ms against \
         {mic_ms}ms"
    );
    // Video follows the arrival clock, so it covers the feed; a second audio input must not disturb it.
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

/// Without a microphone: one untagged audio track and no microphone port.
///
/// End-to-end half of the byte-identical contract (the argument-list half is in `src/ffmpeg.rs`).
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
    assert_eq!(
        encoder.mic_port(),
        None,
        "no microphone, no microphone port"
    );

    let mut video = StubCapture::new(StubConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: FPS,
    });
    video.start().expect("start the capture stub");
    let mut game = Tone::new(GAME_TONE_HZ);
    feed_for(&mut encoder, &mut video, &mut game, None, FEED_WITHOUT_MIC);
    encoder.finish().expect("flush encoder");

    let segments = segments_in(dir.path());
    assert!(!segments.is_empty(), "a segment was written");
    let streams = streams_of(&probe_json(&bin, &segments[0]));

    assert_eq!(
        streams.iter().filter(|s| s.codec_type == "video").count(),
        1,
        "{streams:#?}"
    );
    assert_eq!(
        streams.iter().filter(|s| s.codec_type == "audio").count(),
        1,
        "an encoder spawned without a microphone must produce the single-track output it \
         always did — no second input, no mapping, no tags: {streams:#?}"
    );
    // No mapping means no title metadata: the one audio stream is untagged.
    assert_eq!(
        streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .and_then(|s| s.title.clone()),
        None,
        "the single-track path gains no metadata: {streams:#?}"
    );
}

/// Every way a microphone block can be refused, refused out loud.
///
/// Dropping a block silently leaves a container that says two tracks with an empty or shortened
/// second one. So both refusals are named errors: no second input at all, and a block whose format
/// contradicts what the child was told (48kHz PCM under a 44.1kHz declaration plays 8.8% fast).
#[test]
fn a_microphone_block_the_encoder_cannot_use_is_refused_out_loud() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

    // 1. No microphone input to hand a block to.
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

    // 2. A microphone input, but the block's format differs from the declared one.
    let mut cfg = cfg;
    cfg.mic_audio = Some(MicAudioSpec {
        sample_rate: 44_100,
        channels: 1,
        sample_format: SampleFormat::S16Le,
    });
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

/// The scratch directory's segments, in ffmpeg's order.
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

/// `ffprobe`'s JSON for one file, so the scanner and `MediaInfo` read the same bytes.
fn probe_json(bin: &FfmpegBinaries, path: &Path) -> String {
    let out = Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
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
    /// The stream's title, which tells the two audio tracks apart (see [`track_title`]).
    title: Option<String>,
    /// `stream.duration` in whole milliseconds, when ffprobe reports one.
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
/// A small scanner instead of a JSON dependency: `MediaInfo` keeps only the first audio stream and
/// no tags, and this test needs every stream and its title. It reads the slice of JSON ffprobe
/// emits here (objects of scalars plus one nested `tags` object) and panics on any other shape.
fn streams_of(json: &str) -> Vec<ProbedStream> {
    let streams_at = json
        .find("\"streams\"")
        .expect("ffprobe json has a streams array");
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

/// The title of a stream, as the container carries it.
///
/// ffmpeg's MP4 muxer stores `-metadata:s:a:0 title=...` in the track's `name` atom, so ffprobe
/// reports `"tags": { "handler_name": "SoundHandler", "name": "Game Audio" }`. `name` is read
/// first and `title` accepted too, so a different spelling is reported as itself rather than as
/// "no tag". The needles carry their quotes so `codec_name` and `handler_name` cannot match.
fn track_title(object: &str) -> Option<String> {
    string_field(object, "name").or_else(|| string_field(object, "title"))
}

/// The value of a `"key": "value"` pair inside one stream object, if present.
///
/// Keys searched (`codec_type`, `name`, `title`, `duration`) are unambiguous within one object.
fn string_field(object: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = object.find(&needle)? + needle.len();
    let rest = object[after_key..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Mean volume of one audio stream in a narrow band around `hz`, in dB.
///
/// Behind a narrow `bandpass`, `volumedetect` reads the track's energy at that frequency: tens of
/// dB lower if the tone is absent, `-inf` if silent.
fn band_energy_db(bin: &FfmpegBinaries, path: &Path, stream: usize, hz: f64) -> f64 {
    let filter = format!("bandpass=f={hz}:width_type=h:w={MEASURE_BAND_HZ},volumedetect");
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            &format!("0:a:{stream}"),
            "-af",
            &filter,
            "-f",
            "null",
            "-",
        ])
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

/// Parses `[volumedetect] mean_volume: -23.4 dB`, or `-inf dB` for silence.
fn mean_volume_db(stderr: &str) -> Option<f64> {
    let line = stderr.lines().find(|line| line.contains("mean_volume:"))?;
    let value = line
        .split("mean_volume:")
        .nth(1)?
        .trim()
        .strip_suffix("dB")?
        .trim();
    if value == "-inf" {
        return Some(f64::NEG_INFINITY);
    }
    value.parse().ok()
}
