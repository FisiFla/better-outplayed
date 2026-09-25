//! One ffmpeg process, a pipe video input, **two optional loopback-TCP audio inputs**,
//! segmented MP4 output.
//!
//! Video arrives on `pipe:0` — the child's stdin, which `std::process` hands us as a
//! write end directly, so it needs no socket. Audio arrives on
//! `tcp://127.0.0.1:{port}`: we bind a listener on an ephemeral loopback port and
//! ffmpeg dials it, because for an input URL ffmpeg's tcp protocol is the CLIENT.
//!
//! ## Two audio inputs, one transport, one clock discipline
//!
//! The child declares up to three inputs, in this order:
//!
//! | input | stream | transport |
//! |---|---|---|
//! | 0 | raw BGRA video | `pipe:0` |
//! | 1 | system/game audio | `tcp://127.0.0.1:{game_audio_port}` |
//! | 2 | microphone (`EncodeConfig::mic_audio`) | `tcp://127.0.0.1:{mic_audio_port}` |
//!
//! Input 2 exists only when [`EncodeConfig::mic_audio`] is `Some`, and then the output
//! carries both audio tracks: `-map 0:v -map 1:a -map 2:a`, tagged `Game Audio` and
//! `Microphone`. The microphone is a **second listener bound the same way**, pumped by the
//! same [`pump_audio`] function on its own thread — its timing conventions are not a
//! reimplementation but the same code: PCM is written as it is submitted, the media timeline
//! is the sample count, starvation shows up as a stall in ffmpeg's own input, and a full
//! queue drops and counts a block rather than blocking the capture loop
//! ([`enqueue_or_drop`]). One writer thread *per input* is load-bearing, not tidiness:
//! ffmpeg will not pull one input far ahead of another, so writing two inputs
//! synchronously from one thread deadlocks the moment either sink's buffer fills.
//!
//! ## Why audio does not come in on `pipe:1`
//!
//! Writing audio into the child's stdout (`-i pipe:1`) is a Unix-only trick, and an
//! accident of POSIX rather than a supported interface. ffmpeg's pipe protocol parses
//! the descriptor number out of the URL and then calls a plain CRT `read(fd, ...)`.
//! Windows opens descriptor 1 as write-only, so `read(1, ...)` fails with `EBADF` and
//! the audio input is dead on arrival. POSIX happens to permit reading fd 1, which is
//! precisely why the existing suite passed on macOS while the transport was broken on
//! the platform this project actually ships to. The trick was never portable.
//!
//! ## Why not Windows named pipes
//!
//! Named pipes would work on Windows, but the development host is macOS and cannot
//! exercise them: the transport would ship unverified on the very platform it exists
//! for. ffmpeg's `tcp://` protocol is supported on every platform, so loopback TCP
//! collapses this to ONE code path with no `#[cfg]` — and that path is genuinely
//! exercised by the macOS test suite, which is the only place it can be run here.
//!
//! ## This is IPC, not egress
//!
//! The listener is explicitly bound to `127.0.0.1` (never a wildcard, never a public
//! interface) and ffmpeg connects back to that same address, so neither the port nor
//! the peer can leave the host. The project's zero-egress rule (spec §7.3, enforced by
//! `crates/events/tests/no_egress.rs`) is about OUTBOUND traffic; two processes of
//! this application talking over the loopback interface are not egress. The accepted
//! connection's peer is checked to be a loopback address as well, so a stray
//! connection cannot silently feed the encoder.
//!
//! ## The media timeline is the wall clock, not the frame count
//!
//! A capture source delivers frames at whatever rate it manages — WGC hands over one
//! frame per compositor tick, which on a 36fps-ish delivery is *not* the configured
//! `encode.fps`. If the encoder assigned timestamps from a declared frame rate
//! (`-r 30`) the media timeline would advance at `frames / 30` while the wall clock
//! advanced at `frames / 36`, and the two would drift apart without bound: measured on
//! real hardware, 25.4s of wall clock produced 19.0s of media (919 frames, `span=19000ms`
//! against `need=28690ms`), so `trigger_ms + post_ms` could never be reached and every
//! hotkey press timed out with "the encoder produced no segment covering the trigger".
//!
//! Video input timestamps are therefore taken from the wall clock at the moment each
//! frame is *read* (`-use_wallclock_as_timestamps 1`), which is the arrival time of the
//! frame. Segments then cover wall-clock time — `segments * segment_time` is real
//! seconds — even when the delivery rate differs from the configured one, and a stall in
//! capture shows up as a gap in the timeline rather than as a slower-than-real-time clock.
//! The capture loop additionally rate-limits itself to `encode.fps` (see `FramePacer` in
//! the CLI), so in the ordinary case the two agree and no frames are wasted.
//!
//! The nominal rate is declared with the rawvideo demuxer's own `-framerate`, NOT with
//! the CLI's input `-r`. This is not cosmetic: `-r` before `-i` sets the CLI's notion of
//! an input frame rate, and ffmpeg then *re-stamps* every decoded frame onto a rigid
//! 1/fps grid, throwing the arrival time away — measured with `-debug_ts`, a pipe fed at
//! 15fps with wallclock stamps and input `-r 30` reached the muxer as pts 0, 1/30, 2/30,
//! … while the same pipe with `-framerate 30` reached it as 0, 0.100, 0.233, … i.e. the
//! real arrival times. `-r` silently undoes the whole fix, so it must not be used here.
//!
//! Audio is deliberately *not* stamped that way. Its input is raw PCM whose timeline is
//! already exact: s16le at 48kHz, muxed from the running sample count, gives 1/48000 s
//! per sample with no accumulation error and no dependence on when a block happened to
//! be read off the socket (and it is drained in full rather than rate-limited, because
//! dropping audio to pace video would desync the clip). Stamping audio from the wall
//! clock would replace that exact timeline with socket-arrival jitter. The two streams
//! are reconciled by the same clock the trigger uses — the capture loop's — so the small
//! drift between the audio device's crystal and the system clock is what remains, and
//! `MediaInfo::av_drift` measures it on every clip.
//!
//! ## Threading
//!
//! Each input is drained by its own writer thread, fed by a **bounded** `mpsc` channel,
//! so `submit_video`/`submit_audio` never block the caller. ffmpeg will not pull one
//! input far ahead of the other (its muxer buffers to interleave audio and video), so
//! writing both streams in bursts from the calling thread deadlocks as soon as either
//! sink's buffer fills. Decoupling the two writes lets the submits arrive in any order.
//!
//! The payload's buffer is **moved** into the channel, not cloned: `Frame`/`AudioBuffer`
//! are taken by value in the trait for exactly that reason. The clone this replaced
//! copied the whole pixel buffer on every frame — 3840x2160 BGRA is 33.2MB, ~1GB/s of
//! pure memcpy at 30fps — on the path that was already failing to keep up.

use crate::{EncodeConfig, EncodeOutput, Encoder, SampleFormat, VideoInput};
use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBuffer, Frame};
use localplay_media::FfmpegBinaries;
use std::ffi::OsString;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the audio thread waits for ffmpeg to dial the loopback input before
/// giving up. ffmpeg connects while opening its inputs, well before it reads a frame
/// of video, so this only elapses when the child failed to start at all.
const AUDIO_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for that connection. The listener is non-blocking, so
/// we re-check instead of sitting in a blocking `accept()` that could never return.
const AUDIO_CONNECT_POLL: Duration = Duration::from_millis(20);

/// How long a failure report waits for ffmpeg's stderr before giving up on including it.
/// The text is normally already sitting in the pipe buffer, so this is a ceiling on a
/// pathological case, not a delay on the ordinary one (see `drain_stderr`).
const STDERR_DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// How long a writer failure waits for ffmpeg to become reapable, so its exit status and
/// stderr can be reported as the cause (see `FfmpegEncoder::ffmpeg_death`).
///
/// The broken pipe is noticed the moment ffmpeg closes its stdin, which can be a few
/// milliseconds before the process itself can be waited on. This is spent only on a path
/// that has already failed, and it is a deadline, not a guarantee: past it the error is
/// reported with the symptom alone.
const CHILD_DEATH_GRACE: Duration = Duration::from_millis(250);

/// Poll interval while waiting out that grace period.
const CHILD_DEATH_POLL: Duration = Duration::from_millis(10);

/// How many video frames may sit in the queue between the capture loop and the writer
/// thread before submits start being dropped.
///
/// The queue is bounded because an unbounded one grows without limit whenever ffmpeg
/// reads slower than capture produces — the opposite of the project's flat-RAM principle,
/// and it fails by exhausting memory rather than by losing frames.
///
/// The number is small on purpose, and its size is dominated by the frame, not by the
/// count: at 3840x2160 BGRA one frame is 3840*2160*4 = 33_177_600 B (~33.2 MB), so four
/// queued frames are ~133 MB, and with the fifth frame the writer thread holds while
/// blocked inside `write()` the encoder's worst case is ~166 MB. The slack that buys is
/// also small on purpose: at 30fps a frame is 33 ms, so four frames ride out ~133 ms of
/// ffmpeg not reading — long enough to cover a segment being finalised (moov written,
/// file closed, next one opened) without dropping anything, far short of letting a
/// wedged encoder accumulate a gigabyte.
const VIDEO_QUEUE_FRAMES: usize = 4;

/// How many 10 ms audio blocks may be queued before audio submits start being dropped.
///
/// Blocks are 1920 B at 48kHz stereo s16le, so this is ~61 KB — 320 ms of audio, ~1600x
/// cheaper per millisecond of slack than the video queue. Audio is given more slack than
/// video for that reason: a dropped block is a hole in the sound, and the memory saved by
/// trimming this number would be noise. The microphone's queue (`FfmpegEncoder::mic_tx`) is
/// sized the same way, because it carries the same kind of payload.
const AUDIO_QUEUE_BLOCKS: usize = 32;

/// The format the **game/system** audio input has always been declared with: raw s16le PCM
/// at 48kHz stereo.
///
/// Named rather than inlined because the microphone input is declared by the same function
/// ([`audio_input_args`]): two inputs declared by one piece of code cannot drift from each
/// other, which is what makes "both audio streams share one clock discipline" mechanical
/// rather than a promise. These are the pipeline's published values
/// (`localplay_capture::AudioFormat::default`), not settings.
const GAME_AUDIO_SAMPLE_RATE: u32 = 48_000;
const GAME_AUDIO_CHANNELS: u16 = 2;

/// What an audio input is called in error messages and in the writer thread's name.
///
/// The game audio keeps the exact wording every existing report already uses, so a
/// single-audio failure reads as it did before the microphone existed; the second input
/// names itself, which is the difference between "the audio input failed" and "the
/// microphone input failed".
const GAME_AUDIO_LABEL: &str = "audio";
const MICROPHONE_LABEL: &str = "microphone";

/// Bytes queued by the caller, written to the child by a dedicated thread.
type WriterHandle = JoinHandle<std::io::Result<()>>;

/// Seconds per scratch segment — which is also the forced-keyframe interval, and therefore
/// what makes a clip a lossless concatenation of whole segments (spec §6.3).
pub fn keyframe_seconds(cfg: &EncodeConfig) -> f64 {
    cfg.segment_ms as f64 / 1000.0
}

/// The GOP that puts a keyframe at every segment boundary.
fn gop_frames(cfg: &EncodeConfig) -> u32 {
    (cfg.fps as f64 * keyframe_seconds(cfg)).round().max(1.0) as u32
}

/// The ffmpeg arguments that declare the **video input and its rate**.
///
/// Shared by the two places that drive this encoder — the long-lived child that records
/// ([`FfmpegEncoder::spawn`]) and the startup throughput probe that measures what the
/// machine can sustain before it is asked to record anything
/// (`crate::throughput::measure_sustainable_fps`) — because a measurement of a *different*
/// ffmpeg invocation is not a measurement of this one. The geometry, the pixel format and
/// above all the declared rate have to be the same on both sides for the number the probe
/// returns to mean anything about the recording that follows.
///
/// `-framerate` is the rawvideo demuxer's own option and deliberately NOT the CLI's input
/// `-r`: `-r` makes ffmpeg treat the input as constant-rate and re-stamp every frame onto a
/// rigid 1/fps grid, which discards the arrival timestamp the next option exists to record
/// (see the module comment for the measurement). Both spellings declare the same nominal
/// rate to the rawvideo demuxer; only this one leaves the real timestamps alone.
pub fn video_input_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = ["-f", "rawvideo", "-pix_fmt", "bgra"].map(str::to_string).to_vec();
    // `-s` sizes the incoming rawvideo stream, so it must be the SOURCE size (what the
    // capture backend delivers), never the encode output size.
    args.extend(["-s".to_string(), format!("{}x{}", cfg.source_size.0, cfg.source_size.1)]);
    // The nominal rate: what the pipeline declares the stream to be. It is one number for
    // both consumers — this argument and the pacer that feeds the pipe — see
    // `localplay_recorder::FpsDecision`.
    args.extend(["-framerate".to_string(), cfg.fps.to_string()]);
    // Timestamps come from the moment each frame is read, i.e. its arrival time, not from a
    // declared frame rate. This is what keeps the media timeline glued to the wall clock:
    // `segments * segment_time` stays real seconds even when capture delivers at a rate
    // other than `cfg.fps`, so the post-roll the hotkey waits for is actually reached.
    // Video only — audio's timeline is the exact 48kHz sample count and must not be
    // jittered by socket arrival (module comment: "The media timeline is the wall clock,
    // not the frame count").
    args.extend(["-use_wallclock_as_timestamps".to_string(), "1".to_string()]);
    args.extend(["-i".to_string(), "pipe:0".to_string()]);
    args
}

/// The ffmpeg arguments for an **already-encoded** video input: an H.264 elementary stream on a
/// pipe, produced by this process rather than by ffmpeg.
///
/// This is the Tier 2 hybrid's video half. A hardware encoder MFT takes the captured texture
/// straight from VRAM (`localplay_encoder::mft`), and what crosses the pipe is a few hundred
/// kilobytes a second of H.264 instead of ~2 GB/s of raw pixels — measured on the box: 90 frames of
/// 4K `ARGB32` in, an elementary stream out, no CPU copy.
///
/// The timeline flag is the same one, for the same reason, as [`video_input_args`]: an elementary
/// stream carries no timestamps of its own, so ffmpeg has to be told to take them from when the
/// bytes arrive. Measured on the box, both ways: streaming the encoder's output as it is produced
/// gives **965 ms of container for 990 ms of paced frames**, and handing the same bytes over in one
/// batch gives **196 ms**, because a batch makes every frame arrive at once.
pub fn video_input_args_bitstream() -> Vec<String> {
    [
        "-f",
        "h264",
        "-use_wallclock_as_timestamps",
        "1",
        "-i",
        "pipe:0",
    ]
    .map(str::to_string)
    .to_vec()
}

/// The ffmpeg arguments for what to do with that input: **nothing**. The video is copied through,
/// because the pixels were encoded on the GPU and never reached this process's memory.
///
/// Deliberately almost empty, and the absences are the substance:
///
/// * no `-c:v h264_*` and no `-b:v` — there is nothing left to encode, and the bitrate belongs to
///   the MFT.
/// * no `-g` and no **`-force_key_frames`**: neither can apply to a stream ffmpeg is only copying.
///   The segment boundaries the replay path depends on therefore have to come from the encoder,
///   which is why `MftEncoder` sets `MF_MT_MAX_KEYFRAME_SPACING` — measured on the box as three
///   fragments a second apart, each a keyframe, read back by the project's own `FragmentSplitter`.
/// * no `-fps_mode`: it describes a conversion onto a grid, and a copied stream has no frames to
///   convert.
/// * no `-vf scale`: scaling means decoding, decoding means the pixels, and the pixels are exactly
///   what this path exists to keep out of this process. **A scaled output therefore has to be asked
///   of the capture, not of ffmpeg** — so `encode.output_size` below the capture size is a reason to
///   capture at that size, not to filter here.
pub fn video_output_args_bitstream() -> Vec<String> {
    vec!["-c:v".to_string(), "copy".to_string()]
}

/// The ffmpeg arguments that describe **the encoder for that video input**: how frames are
/// passed to it, the scale filter when the output differs from the capture, the codec, its
/// bitrate and its keyframe schedule.
///
/// Shared with the throughput probe for the same reason as [`video_input_args`]: the probe
/// has to pay for the work the recording pays for. It writes to the null muxer, so the
/// forced keyframes and the scale cost it exactly what they cost the segmenter.
pub fn video_output_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // THE load-bearing option of the whole timeline. Do not remove it.
    //
    // ffmpeg's default (`-fps_mode auto`) picks a constant-rate conversion for this output,
    // and a constant-rate conversion can only produce a rigid `1/R` grid — so a stream whose
    // frames arrive at anything other than the declared `-framerate R` gets *resampled onto
    // that grid*: missing frames are free to invent (duplicated), surplus ones are dropped.
    // With the segment muxer in play that is not a cosmetic metadata difference, it IS the
    // media clock. Measured on the dev host with the real argument list (4K output,
    // libx264, ~8 frames a second arriving against a declared 120): 1631 frames were
    // encoded from 39 delivered, `dup=1592`, every segment reported `avg_frame_rate=120/1`,
    // and the ring's media clock then advanced at (frames the encoder can emit) ÷ R rather
    // than at the wall clock — the segments themselves appeared at 0.66 per second, so
    // `span=` fell behind real time for ever (issue #2).
    //
    // `passthrough` removes the conversion: each frame reaches the muxer with the timestamp
    // it already has, which for this input is its arrival time (`-use_wallclock_as_timestamps
    // 1`, see [`video_input_args`]). Media time then advances with the wall clock *by
    // construction*, whatever rate frames arrive at, and a machine that cannot keep up drops
    // frames — visible as holes in the picture and as a lower `avg_frame_rate` — instead of
    // silently stretching the clock. `-vsync 0` is the same option's older spelling; the
    // modern one is used because ffmpeg 5.1 renamed it (and the pinned Windows sidecar
    // carries the new name: `strings binaries/ffmpeg.exe` finds `fps_mode` and its
    // error text).
    args.extend(["-fps_mode".to_string(), "passthrough".to_string()]);
    // Frames arrive at `source_size`; when the caller asked for a different output size,
    // scale to it. Equal sizes add no filter, which avoids a pointless pass.
    if cfg.source_size != cfg.output_size {
        args.extend([
            "-vf".to_string(),
            format!("scale={}:{}", cfg.output_size.0, cfg.output_size.1),
        ]);
    }
    args.extend(["-c:v".to_string(), cfg.encoder_name().to_string()]);
    args.extend(["-b:v".to_string(), format!("{}k", cfg.bitrate_kbps)]);
    args.extend(["-g".to_string(), gop_frames(cfg).to_string()]);
    // Forced keyframes are what make segment boundaries cuttable (spec §6.3).
    args.extend([
        "-force_key_frames".to_string(),
        format!("expr:gte(t,n_forced*{})", keyframe_seconds(cfg)),
    ]);
    args
}

/// The ffmpeg arguments that declare **one raw-PCM loopback audio input**: its format, its
/// sample rate, its channel count and the URL ffmpeg dials.
///
/// Both audio inputs are declared by this function — the game/system one from the constants
/// above, the microphone from [`crate::MicAudioSpec`] — so the two cannot be declared
/// differently: same demuxer, same option order, same meaning for every number. That is the
/// "one clock discipline" of the module comment applied to the *transport*, and it is also
/// what makes the microphone's format a checked value rather than a copy of a literal: a
/// rate or channel count that disagrees with the PCM actually submitted shifts the
/// microphone track against the other two streams instead of failing.
fn audio_input_args(
    url: &str,
    sample_rate: u32,
    channels: u16,
    sample_format: SampleFormat,
) -> Vec<String> {
    vec![
        "-f".to_string(),
        sample_format.ffmpeg_name().to_string(),
        "-ar".to_string(),
        sample_rate.to_string(),
        "-ac".to_string(),
        channels.to_string(),
        "-i".to_string(),
        url.to_string(),
    ]
}

/// The **complete** ffmpeg argument list for one recording child, program name excepted.
///
/// Every argument this encoder ever passes is built here, as a pure function of the
/// configuration and the two loopback URLs, and [`FfmpegEncoder::spawn`] does nothing with
/// the result but hand it to `Command::args`. That is deliberate: it is what lets a test
/// assert the whole argument list element for element, and in particular assert the
/// microphone's contract — with [`EncodeConfig::mic_audio`] `None`, this vector is what it
/// was before the second input existed, byte for byte
/// (`the_argument_list_without_a_microphone_is_byte_identical_to_the_pre_change_list`).
///
/// `OsString` rather than `String` because the segment pattern at the end is a real path:
/// an `EncodeConfig::scratch_dir` that is not valid UTF-8 must reach ffmpeg as the bytes it
/// actually is. `Command::args` takes them as-is either way.
fn ffmpeg_args(cfg: &EncodeConfig, audio_url: &str, mic_url: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> =
        ["-hide_banner", "-loglevel", "error", "-nostdin"].iter().map(OsString::from).collect();
    // (`-nostdin` is what keeps ffmpeg from consuming our stdin for interactive commands,
    // which would steal raw video frames; `-loglevel error` is why a failure's reason is
    // read out of stderr by `drain_stderr` rather than scraped from a progress line.)

    // Input 0 — video, in one of two shapes, and which one is the whole of Tier 2. Raw BGRA frames
    // this process copied out of VRAM ([`video_input_args`], shared with the throughput probe), or
    // an H.264 elementary stream it encoded *on the GPU* ([`video_input_args_bitstream`]). See
    // [`VideoInput`] for what each costs — at 4K the difference is ~33 MB per frame against a few
    // hundred kilobytes a second.
    args.extend(
        match cfg.video {
            VideoInput::RawPixels => video_input_args(cfg),
            VideoInput::EncodedBitstream => video_input_args_bitstream(),
        }
        .into_iter()
        .map(OsString::from),
    );

    // Input 1 — system/game audio: raw s16le PCM over loopback TCP. This is the transport
    // that works on Windows as well as here — see the module comment.
    args.extend(
        audio_input_args(
            audio_url,
            GAME_AUDIO_SAMPLE_RATE,
            GAME_AUDIO_CHANNELS,
            SampleFormat::S16Le,
        )
        .into_iter()
        .map(OsString::from),
    );

    // Input 2 — the microphone, when one was configured. Declared by the same function as
    // input 1 and pumped by the same function too, so the second track cannot invent its
    // own timing conventions; only the numbers come from the caller's `MicAudioSpec`.
    if let Some(mic_url) = mic_url {
        let spec = cfg.mic_audio.expect(
            "a microphone URL is only ever passed when EncodeConfig::mic_audio is Some",
        );
        args.extend(
            audio_input_args(mic_url, spec.sample_rate, spec.channels, spec.sample_format)
                .into_iter()
                .map(OsString::from),
        );
    }

    // What to do with input 0: scale (when asked for), codec, bitrate and the keyframe schedule —
    // or, for a stream that arrived encoded, nothing but a copy. Shared with the probe for the same
    // reason.
    args.extend(
        match cfg.video {
            VideoInput::RawPixels => video_output_args(cfg),
            VideoInput::EncodedBitstream => video_output_args_bitstream(),
        }
        .into_iter()
        .map(OsString::from),
    );
    args.extend(["-c:a", "aac", "-b:a"].iter().map(OsString::from));
    args.push(format!("{}k", cfg.audio_bitrate_kbps).into());

    // Only with a second audio input does the output have to be mapped explicitly: with
    // ONE audio input ffmpeg's default stream selection already picks exactly input 0's
    // video and input 1's audio, which is why the single-track list has never carried a
    // `-map` (and why it still must not: with `mic_audio: None` the list is unchanged).
    // With two audio inputs the default would pick ONE of them, so both are mapped and both
    // are tagged — a two-track container whose tracks cannot be told apart is not the
    // feature. Input order is the source of both indices: 1 is the game audio, which
    // `Encoder::submit_audio` feeds, and 2 the microphone, which `Encoder::submit_mic_audio`
    // feeds.
    if mic_url.is_some() {
        args.extend(["-map", "0:v", "-map", "1:a", "-map", "2:a"].iter().map(OsString::from));
        // The two tag strings come from `localplay-media`, which has to apply the same names
        // again when it concatenates segments into a clip or a session file: a `-c copy` does
        // not carry per-stream metadata, so these names are written twice in a recording's
        // life, and two literals free to drift is exactly how a track ends up anonymous in the
        // file the user opens.
        for (index, title) in localplay_media::edit::audio_titles(2).iter().enumerate() {
            args.push(format!("-metadata:s:a:{index}").into());
            args.push(format!("title={title}").into());
        }
    }

    match cfg.output {
        EncodeOutput::Segmented => {
            args.extend(["-f", "segment"].iter().map(OsString::from));
            args.extend(["-segment_time"].iter().map(OsString::from));
            args.push(keyframe_seconds(cfg).to_string().into());
            args.extend(["-segment_format", "mp4"].iter().map(OsString::from));
            // Each segment starts at zero, which is what the concat at clip time relies on
            // (spec §6.3): every segment is a self-contained unit starting at t=0.
            args.extend(["-reset_timestamps", "1"].iter().map(OsString::from));
            // Continue the numbering instead of restarting it. ffmpeg's segment muxer supports
            // this as `segment_start_number`; plain `-start_number` is *not* an option of this
            // muxer and is silently ignored (measured: with `-start_number 5` the first file was
            // still `seg-000000.mp4`), which would leave the encoder overwriting files the adopted
            // ledger still names.
            args.extend(["-segment_start_number"].iter().map(OsString::from));
            args.push(cfg.start_number.to_string().into());
            args.push(cfg.scratch_dir.join(SEGMENT_PATTERN).into());
        }
        EncodeOutput::FragmentedStream => {
            // A stream, not files: fragmented MP4 on the child's stdout, so nothing is written
            // to the SSD while the user is merely waiting for something worth clipping.
            //
            // `frag_keyframe` is what keeps `segment_ms` meaningful here. It starts a new
            // fragment on every keyframe, and `video_output_args` already forces one per
            // `segment_ms` (`-force_key_frames expr:gte(t,n_forced*…`) for the segmenter's
            // sake — so a fragment boundary IS a segment boundary, the same cut granularity
            // from the same interval, with no second keyframe argument to keep in step.
            // Without it ffmpeg would fragment on its own schedule and a range of fragments
            // would not line up with the trigger.
            //
            // `empty_moov` + `default_base_moof` make the stream self-describing from the
            // first byte and free of a seek-back-to-patch-the-header requirement, which is
            // what a pipe cannot provide. Measured shape: a 1249-byte `ftyp`+`moov`, then one
            // `moof`+`mdat` per second carrying a monotonic `tfdt`.
            args.extend(["-f", "mp4"].iter().map(OsString::from));
            args.extend(
                ["-movflags", "empty_moov+frag_keyframe+default_base_moof"]
                    .iter()
                    .map(OsString::from),
            );
            args.push("pipe:1".into());
        }
    }

    args
}

/// The segment muxer's file-name pattern inside [`EncodeConfig::scratch_dir`].
const SEGMENT_PATTERN: &str = "seg-%06d.mp4";

pub struct FfmpegEncoder {
    child: Child,
    /// The child's stdout, when it was spawned with [`EncodeOutput::FragmentedStream`].
    ///
    /// Taken by the first caller that asks (`Encoder::take_output_stream`), because a pipe has
    /// exactly one reader: whoever holds it owns the stream, and the buffer that keeps the last
    /// minute of footage in memory is the thing that should.
    output_stream: Option<std::process::ChildStdout>,
    video_tx: Option<SyncSender<Vec<u8>>>,
    audio_tx: Option<SyncSender<Vec<u8>>>,
    /// The microphone's queue into its own pump thread. `None` when the child was spawned
    /// without a second audio input — see [`EncodeConfig::mic_audio`].
    mic_tx: Option<SyncSender<Vec<u8>>>,
    video_writer: Option<WriterHandle>,
    audio_writer: Option<WriterHandle>,
    mic_writer: Option<WriterHandle>,
    encoder_name: &'static str,
    /// The geometry the rawvideo pipe was declared with (`-s {w}x{h}`), reported back to
    /// the caller so a frame of any other size can be refused instead of being sliced
    /// into the pipe at the wrong stride (see `Encoder::source_size`).
    source_size: (u32, u32),
    /// The rate the rawvideo pipe was declared with (`-framerate {fps}`), reported back so
    /// the caller that paces capture can pace to exactly the number the child was told
    /// (see `Encoder::input_fps`).
    input_fps: u32,
    /// The loopback port the microphone input was bound to, reported by
    /// `Encoder::mic_port`. `None` when there is no microphone input.
    mic_port: Option<u16>,
    /// What the child was told the microphone input is (`-f/-ar/-ac`), kept so
    /// [`Encoder::submit_mic_audio`] can refuse a block whose own format disagrees with the
    /// declaration instead of putting the track on a timeline of the wrong length.
    mic_spec: Option<crate::MicAudioSpec>,
    /// Frames dropped because a queue was full. Atomics because the count is written on
    /// the submitting thread and read through `&self` (see `Encoder::dropped_frames`).
    dropped_video: AtomicU64,
    dropped_audio: AtomicU64,
    /// Microphone blocks dropped, for the same reason and by the same mechanism as
    /// [`FfmpegEncoder::dropped_audio`].
    dropped_mic_audio: AtomicU64,
    /// Text already read out of ffmpeg's stderr, which a pipe can only give up once
    /// (see [`FfmpegEncoder::drain_stderr`]). Two different reports can want it — the
    /// writer failure that explains a dead encoder, and [`Encoder::finish`] — and the
    /// second one must not be left saying nothing.
    drained_stderr: Option<String>,
}

impl FfmpegEncoder {
    pub fn spawn(bin: &FfmpegBinaries, cfg: &EncodeConfig) -> Result<Self> {
        // Refused before anything is created or launched, because this is a *configuration*
        // contradiction rather than a runtime failure and the caller should hear about it as one.
        //
        // An encoded bitstream cannot be scaled here: scaling means decoding, decoding means the
        // pixels, and the pixels are the cost [`VideoInput::EncodedBitstream`] exists to remove. The
        // alternative — quietly ignoring `output_size` and emitting the capture's size — would give
        // the user a recording of the wrong geometry and no indication of why.
        if cfg.video == VideoInput::EncodedBitstream && cfg.source_size != cfg.output_size {
            bail!(
                "an encoded bitstream cannot be scaled by ffmpeg, and this encoder was asked for \
                 {}x{} from {}x{} capture: scaling means decoding, and decoding means the pixels \
                 this mode exists to keep out of this process. Capture at the output size instead, \
                 or ask for an output of the capture's size.",
                cfg.output_size.0,
                cfg.output_size.1,
                cfg.source_size.0,
                cfg.source_size.1
            );
        }
        std::fs::create_dir_all(&cfg.scratch_dir)
            .with_context(|| format!("creating {}", cfg.scratch_dir.display()))?;

        // Audio comes in over loopback TCP. Bind before spawning the child: the port
        // has to be in the argument list, and `:0` makes the OS pick a free one, which
        // we read back from the socket rather than guessing (racing another process
        // for a fixed port is exactly what this avoids).
        let audio_listener = TcpListener::bind("127.0.0.1:0")
            .context("binding the audio input listener on 127.0.0.1")?;
        let audio_port = audio_listener
            .local_addr()
            .context("reading the audio input listener's port")?
            .port();
        let audio_url = format!("tcp://127.0.0.1:{audio_port}");

        // The microphone is a SECOND listener of exactly the same kind, bound at the same
        // point (before the child exists) because its port has to be in the argument list
        // too. Binding here is also what makes "the microphone could not be brought up" a
        // failure of the start rather than a recording that quietly carries one track: a
        // bind that fails returns from `spawn` with the error, and nothing is recorded.
        let mic: Option<(TcpListener, u16)> = match cfg.mic_audio {
            Some(_) => {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .context("binding the microphone input listener on 127.0.0.1")?;
                let port = listener
                    .local_addr()
                    .context("reading the microphone input listener's port")?
                    .port();
                Some((listener, port))
            }
            None => None,
        };
        let mic_url = mic.as_ref().map(|(_, port)| format!("tcp://127.0.0.1:{port}"));

        let mut cmd = Command::new(&bin.ffmpeg);
        cmd
            // The whole argument list, built in one place so that what a test asserts is
            // what the child is spawned with — see [`ffmpeg_args`], which is also where the
            // microphone's input/mapping/tagging lives and where the byte-identical
            // contract for `mic_audio: None` is explained.
            .args(ffmpeg_args(cfg, &audio_url, mic_url.as_deref()))
            .stdin(Stdio::piped())
            // Stdout carries the encoded stream in `FragmentedStream` mode and nothing at all in
            // `Segmented` mode (it used to carry the audio pipe, before that moved to loopback
            // TCP). In `Segmented` mode it is routed to the null device so ffmpeg can never
            // write into *our* stdout, where a stray byte would corrupt a caller's output.
            .stdout(match cfg.output {
                EncodeOutput::Segmented => Stdio::null(),
                EncodeOutput::FragmentedStream => Stdio::piped(),
            })
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} (requested encoder: {})",
                bin.ffmpeg.display(),
                cfg.encoder_name()
            )
        })?;

        // Video goes to the child's stdin; the audio listeners are moved into their
        // threads, which accept ffmpeg's connections there.
        //
        // Stdout is the *output* in `FragmentedStream` mode, so it is taken here and kept for
        // the caller that owns the in-memory buffer. In `Segmented` mode it was routed to the
        // null device and `take()` yields `None`.
        let output_stream = child.stdout.take();
        let video_in = child.stdin.take().context("child stdin unavailable")?;

        let (video_tx, video_rx) = mpsc::sync_channel::<Vec<u8>>(VIDEO_QUEUE_FRAMES);
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_BLOCKS);

        let video_writer = std::thread::Builder::new()
            .name("ffmpeg-video-in".into())
            .spawn(move || pump(video_rx, video_in))
            .context("spawning video writer thread")?;
        let audio_writer = std::thread::Builder::new()
            .name("ffmpeg-audio-in".into())
            .spawn(move || pump_audio(audio_listener, audio_rx, audio_port, GAME_AUDIO_LABEL))
            .context("spawning audio writer thread")?;

        // The microphone pump is a thread of its own, and that is not tidiness: ffmpeg
        // will not pull one input far ahead of the other, so a single thread writing both
        // audio inputs would deadlock as soon as either socket's buffer filled (the module
        // comment on threading; this bug was fixed once already). It runs the same
        // `pump_audio` as the game audio — same accept-with-deadline, same blocking-mode
        // fix, same drop policy — so the two tracks cannot diverge in timing conventions.
        let (mic_tx, mic_writer, mic_port) = match mic {
            Some((listener, port)) => {
                let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_BLOCKS);
                let writer = std::thread::Builder::new()
                    .name("ffmpeg-mic-in".into())
                    .spawn(move || pump_audio(listener, rx, port, MICROPHONE_LABEL))
                    .context("spawning microphone writer thread")?;
                (Some(tx), Some(writer), Some(port))
            }
            None => (None, None, None),
        };

        Ok(Self {
            child,
            output_stream,
            video_tx: Some(video_tx),
            audio_tx: Some(audio_tx),
            mic_tx,
            video_writer: Some(video_writer),
            audio_writer: Some(audio_writer),
            mic_writer,
            encoder_name: cfg.encoder_name(),
            source_size: cfg.source_size,
            input_fps: cfg.fps,
            mic_port,
            mic_spec: cfg.mic_audio,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            dropped_mic_audio: AtomicU64::new(0),
            drained_stderr: None,
        })
    }
}

/// Copy queued chunks into a sink until the sender half is dropped, then close it.
/// Returning ends the function, dropping `sink` and signalling EOF to ffmpeg.
fn pump<W: Write>(rx: mpsc::Receiver<Vec<u8>>, mut sink: W) -> std::io::Result<()> {
    for chunk in rx {
        sink.write_all(&chunk)?;
    }
    sink.flush()
}

/// Accept ffmpeg's connection to the audio input, then stream the queued PCM into it.
///
/// Dropping the socket at the end closes it, which is ffmpeg's EOF on the audio input.
///
/// `what` names the input in anything that fails (`"audio"` for the game/system input,
/// `"microphone"` for the second one). Both inputs run this exact function — same accept
/// with a deadline, same accepted-socket fix, same write loop — which is what the module
/// comment means by one clock discipline: the microphone is not a second implementation of
/// the transport, it is the same one on a different port.
fn pump_audio(
    listener: TcpListener,
    rx: mpsc::Receiver<Vec<u8>>,
    port: u16,
    what: &str,
) -> std::io::Result<()> {
    let stream = accept_within(&listener, port, AUDIO_CONNECT_TIMEOUT, what)?;
    // PCM arrives in small blocks (10ms = 1920 bytes at 48kHz stereo s16le) and ffmpeg
    // is reading them in real time, so Nagle would add nothing but latency here.
    stream.set_nodelay(true).map_err(|e| {
        std::io::Error::new(e.kind(), format!("disabling Nagle on the {what} input socket: {e}"))
    })?;
    pump(rx, stream)
}

/// Accept one connection, giving up after `timeout`.
///
/// The listener is switched to non-blocking and polled rather than parked in a
/// blocking `accept()`. If ffmpeg fails to start — bad arguments, a missing encoder, a
/// child that exited on its own — nothing ever dials the port, and an indefinite
/// `accept()` would hang `finish()` for no stated reason. Polling turns that into a
/// message naming the port and the deadline. `WouldBlock` is the only error that means
/// "not yet"; anything else is a real failure and is returned as-is.
///
/// `what` names the input the listener belongs to (`"audio"` / `"microphone"`) in the
/// failure messages; it changes no behaviour.
fn accept_within(
    listener: &TcpListener,
    port: u16,
    timeout: Duration,
    what: &str,
) -> std::io::Result<TcpStream> {
    listener.set_nonblocking(true).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("making the {what} input listener non-blocking: {e}"),
        )
    })?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Belt and braces for the zero-egress note above: the listener is bound
                // to loopback, so a non-loopback peer would mean our assumption about
                // this socket is wrong, and PCM would be going somewhere unexpected.
                if !peer.ip().is_loopback() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{what} input connection from non-loopback peer {peer}"),
                    ));
                }
                // The listener is non-blocking SOLELY so this poll loop can enforce a
                // connect deadline; that has no business leaking into the stream we
                // hand to the pump. BSD-derived platforms (macOS included) propagate
                // O_NONBLOCK from the listener to the socket `accept()` returns, while
                // Linux does not — so without this line the accepted socket is
                // non-blocking only on the very host this suite runs on. A
                // non-blocking socket makes `write_all` fail with `WouldBlock`
                // (`EAGAIN`, os error 35) the instant ffmpeg's receive buffer fills,
                // which is a race on how fast the pump fills it: the intermittent
                // failure this line fixes, and the one that bit the game-audio input
                // twice (docs/platform-traps.md, trap 1). The microphone listener is a
                // SECOND copy of this accept path, so it needs this line for exactly the
                // same reason — not "probably fine because the audio one works".
                // A blocking accepted socket is the correct design here: the pump runs on
                // its own dedicated thread fed by a bounded channel, so the backpressure
                // of a blocking write can still never propagate to `submit_audio` /
                // `submit_mic_audio` on the caller's thread: the submit drops the block
                // instead of waiting for room (see `enqueue_or_drop`).
                stream.set_nonblocking(false).map_err(|e| {
                    std::io::Error::new(
                        e.kind(),
                        format!("making the accepted {what} input socket blocking: {e}"),
                    )
                })?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "ffmpeg never connected to the {what} input on 127.0.0.1:{port} \
                             within {timeout:?}"
                        ),
                    ));
                }
                std::thread::sleep(AUDIO_CONNECT_POLL);
            }
            Err(e) => return Err(e),
        }
    }
}

impl Encoder for FfmpegEncoder {
    /// Queue the frame for the rawvideo pipe, or drop it if the queue is full.
    ///
    /// The frame is taken by value and its pixel buffer is moved into the queue — that
    /// is what the ownership in the trait signature is for. The clone this replaces
    /// copied 3840x2160 BGRA (33.2MB) per frame, ~1GB/s of pure memcpy at 30fps on the
    /// path that is failing to keep up.
    ///
    /// `try_send` rather than `send`, and a drop rather than an error, because this is a
    /// live capture: this call sits in the same loop that has to keep pulling frames off
    /// the capture backend, so blocking here would take the latency budget from a stream
    /// that cannot be paused, and failing would stop recording because the GPU fell a few
    /// milliseconds behind. Dropping the frame is the correct behaviour for a live
    /// stream — the timeline is the wall clock (module comment), so a missing frame is a
    /// brief repeat of its predecessor, not a shift of everything after it.
    ///
    /// The count is what stops the drop from being silent: `Encoder::dropped_frames`
    /// reports it and the CLI logs it, so a soak sees the loss instead of inferring it.
    ///
    /// A disconnected channel is a different matter — the writer thread is gone, ffmpeg
    /// is not reading, and nothing submitted afterwards can be encoded, so that is an
    /// error rather than a drop. That error carries ffmpeg's own exit status and stderr
    /// when they are known by then, because "video writer thread has stopped" is the
    /// symptom, not the cause ([`FfmpegEncoder::explain_dead_writer`]).
    fn submit_video(&mut self, frame: Frame) -> Result<()> {
        // The frame's geometry is checked by the caller (`pump_once_counted`), which is
        // the only place that holds both the frame and the pipe's declared size.
        //
        // What the caller cannot check is whether the frame has **pixels** at all. A frame whose
        // pixels stayed in VRAM has an empty `data` by design (`localplay_capture::Frame`), and
        // this encoder writes `data` to the child's stdin — so accepting one would feed ffmpeg a
        // zero-length rawvideo frame, which it would read as the *next* frame's leading bytes.
        // The result is a stream desync that surfaces as corruption seconds later with nothing
        // pointing back here, which is exactly the class of failure worth an explicit refusal.
        if !frame.has_pixels() {
            bail!(
                "this frame's pixels are not in CPU memory, and this encoder feeds ffmpeg a raw                  byte stream: a zero-length frame would be read as the next frame's bytes. A                  frame must carry pixels to reach this encoder."
            );
        }
        let queued = match self.video_tx.as_ref() {
            Some(tx) => enqueue_or_drop(tx, frame.data, &self.dropped_video, "video"),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    /// Queue an audio block, or drop it if the queue is full. Same reasoning as
    /// [`Encoder::submit_video`]; the counter is `Encoder::dropped_audio_blocks`.
    fn submit_audio(&mut self, audio: AudioBuffer) -> Result<()> {
        let queued = match self.audio_tx.as_ref() {
            Some(tx) => enqueue_or_drop(tx, audio.data, &self.dropped_audio, "audio"),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    /// Queue a **microphone** block on the second input, or drop it if that queue is full.
    /// Same reasoning and the same drop counting as [`Encoder::submit_audio`]; the counter
    /// is `Encoder::dropped_mic_audio_blocks`.
    ///
    /// Unlike the game audio (whose format is a constant of the pipeline), the microphone's
    /// format is what the *caller* declared in `EncodeConfig::mic_audio`, and it is what the
    /// child was told (`-f/-ar/-ac`). A block whose own format disagrees with that
    /// declaration is refused rather than queued: the bytes would be read as a different
    /// length, so the microphone track would sit on a timeline of the wrong length for the
    /// whole clip and nothing downstream could tell. Same failure mode as a frame of the
    /// wrong geometry on the video pipe (`Encoder::source_size`), so the same answer.
    fn submit_mic_audio(&mut self, audio: AudioBuffer) -> Result<()> {
        let Some(tx) = self.mic_tx.as_ref() else {
            bail!(
                "this encoder was spawned without a microphone input \
                 (EncodeConfig::mic_audio was None); the block was refused rather than \
                 silently dropped"
            );
        };
        let spec = self
            .mic_spec
            .expect("a microphone queue exists only when a microphone spec was configured");
        if audio.format.sample_rate != spec.sample_rate || audio.format.channels != spec.channels
        {
            bail!(
                "microphone block is {}Hz {}ch but the input was declared as {}Hz {}ch \
                 (EncodeConfig::mic_audio); accepting it would put the microphone track on \
                 a timeline of the wrong length",
                audio.format.sample_rate,
                audio.format.channels,
                spec.sample_rate,
                spec.channels
            );
        }
        let queued = enqueue_or_drop(tx, audio.data, &self.dropped_mic_audio, MICROPHONE_LABEL);
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    fn finish(&mut self) -> Result<()> {
        // Dropping the senders ends each pump loop, closing the video pipe and the
        // audio sockets — which is what tells ffmpeg the inputs have ended.
        self.video_tx.take();
        self.audio_tx.take();
        self.mic_tx.take();
        // Join every writer before reporting: an audio thread may still be waiting
        // out its connect deadline, and abandoning it would leave a listener that a
        // later connection could reach after this encoder is done.
        let video_res = join_writer(self.video_writer.take(), "video");
        let audio_res = join_writer(self.audio_writer.take(), GAME_AUDIO_LABEL);
        let mic_res = join_writer(self.mic_writer.take(), MICROPHONE_LABEL);

        // The connect deadline is the one failure that leaves ffmpeg alive but silent:
        // it never opened the input, so it never read video either, and waiting on it
        // below would block forever. Kill it, then report the deadline along with
        // whatever it managed to log. Either audio input hitting that deadline has the
        // same consequence and the same handling — and past this point the report names
        // which input it was, because `pump_audio` puts the label in the message.
        let deadline = [audio_res.as_ref().err(), mic_res.as_ref().err()]
            .into_iter()
            .flatten()
            .find(|err| is_connect_deadline(err));
        if let Some(err) = deadline {
            let _ = self.child.kill();
            let status = self.child.wait().context("reaping ffmpeg")?;
            let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
            bail!(
                "{err:#}; ffmpeg status after the deadline was {status}, stderr: {}",
                stderr.trim()
            );
        }

        // ffmpeg's own exit status and stderr explain a failed start (a bad encoder
        // name, for instance); the writer threads only ever see the symptom — a broken
        // pipe. Report the cause first, and fall back to the writer errors.
        let status = self.child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
            bail!(
                "ffmpeg exited with {status} using encoder '{}': {}",
                self.encoder_name,
                stderr.trim()
            );
        }
        video_res?;
        audio_res?;
        mic_res?;
        Ok(())
    }

    fn active_encoder(&self) -> &'static str {
        self.encoder_name
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn input_fps(&self) -> u32 {
        self.input_fps
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped_video.load(Ordering::Relaxed)
    }

    fn dropped_audio_blocks(&self) -> u64 {
        self.dropped_audio.load(Ordering::Relaxed)
    }

    fn dropped_mic_audio_blocks(&self) -> u64 {
        self.dropped_mic_audio.load(Ordering::Relaxed)
    }

    fn mic_port(&self) -> Option<u16> {
        self.mic_port
    }

    fn take_output_stream(&mut self) -> Option<std::process::ChildStdout> {
        self.output_stream.take()
    }
}

impl FfmpegEncoder {
    /// Complete a writer failure with ffmpeg's own exit status and stderr, if it has died.
    ///
    /// The writer thread can only report the symptom: its `write` to the child's stdin
    /// failed, so it stopped, so the channel is disconnected. It does not own the child,
    /// and the fact that arrives at the caller is "…writer thread has stopped" — which is
    /// exactly the unhelpful message a Windows capture produced when `h264_amf` could not
    /// start (`Error: video writer thread has stopped`, with the real cause — a missing
    /// `amfrt64.dll` — only ever in ffmpeg's stderr). This encoder owns the child, so it
    /// can add the cause.
    ///
    /// Returns the error untouched when the child has not exited yet. That is a real
    /// possibility (the pipe can break before the process is reapable) and inventing a
    /// cause would be worse than reporting the symptom honestly.
    fn explain_dead_writer(&mut self, err: anyhow::Error) -> anyhow::Error {
        match self.ffmpeg_death() {
            Some(cause) => anyhow::Error::msg(format!("{err}; {cause}")),
            None => err,
        }
    }

    /// ffmpeg's exit status and stderr, when it has exited.
    ///
    /// A short bounded grace period is spent waiting for the exit first. The broken pipe
    /// is observed the instant ffmpeg closes its stdin, which can precede the moment the
    /// process becomes reapable, and the whole point is to catch the cause. The wait only
    /// happens on a path that has already failed — the run is ending either way — and it
    /// is bounded, so a child that survives the grace period still produces an error.
    ///
    /// `try_wait` reaps the child; a later [`Encoder::finish`] still gets the same status
    /// from `Child::wait`, which returns the stored one.
    fn ffmpeg_death(&mut self) -> Option<String> {
        let deadline = Instant::now() + CHILD_DEATH_GRACE;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(CHILD_DEATH_POLL),
                _ => return None,
            }
        };
        let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
        Some(format!(
            "ffmpeg exited with {status} using encoder '{}': {}",
            self.encoder_name,
            stderr.trim()
        ))
    }

    /// Everything ffmpeg wrote to stderr, but never at the cost of waiting longer than
    /// `budget` for it.
    ///
    /// A plain `read_to_string` blocks until EVERY write end of the pipe is closed.
    /// Normally the child holds the only one, so the read ends where the child does —
    /// but a descendant that inherited the pipe and outlives a killed child keeps it
    /// open, and the read would then hang the error path (measured: a stand-in that
    /// forked a sleeper stalled the report for its whole 60s). Draining on its own
    /// thread and taking whatever arrived keeps the report honest and bounded; on the
    /// ordinary path the text is already buffered and this returns at once.
    ///
    /// The text is remembered because a pipe can only be drained once, and more than one
    /// report wants it: the writer failure that explains a dead encoder, and
    /// [`Encoder::finish`]'s own failure. Without the cache the second one would print
    /// nothing where ffmpeg's reason should be.
    fn drain_stderr(&mut self, budget: Duration) -> String {
        if let Some(text) = &self.drained_stderr {
            return text.clone();
        }
        let Some(mut stderr) = self.child.stderr.take() else {
            return String::new();
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = tx.send(text);
        });
        let text = rx.recv_timeout(budget).unwrap_or_default();
        self.drained_stderr = Some(text.clone());
        text
    }
}

/// Whether a writer error is an audio thread's connect deadline.
///
/// `accept_within` is the only place in this module that raises `TimedOut`, and the two
/// audio threads are its only callers (one per audio input), so the error kind is the
/// signal — and the message it carries names which input it was.
fn is_connect_deadline(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::TimedOut)
}

/// Hand a payload to a writer thread, or drop it if the queue is full.
///
/// `try_send` rather than `send`, and a drop rather than an error, because this is called
/// from the live capture path: blocking here would stall the loop that has to keep pulling
/// frames off the capture backend, and failing would stop recording because the GPU fell a
/// few milliseconds behind. Dropping is the correct behaviour for a live stream — the
/// media timeline is wall-clock (module comment), so a missing payload is a brief repeat
/// of its predecessor rather than a shift of everything after it. The counter is what
/// stops the drop from being silent: it is reported by `Encoder::dropped_frames` /
/// `Encoder::dropped_audio_blocks` and logged by the CLI.
///
/// A disconnected channel is a different matter — the writer thread is gone, ffmpeg is not
/// reading, and nothing submitted afterwards can be encoded, so that is an error.
fn enqueue_or_drop<T>(tx: &SyncSender<T>, item: T, dropped: &AtomicU64, what: &str) -> Result<()> {
    match tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => bail!("{what} writer thread has stopped"),
    }
}

fn join_writer(handle: Option<WriterHandle>, what: &str) -> Result<()> {
    let Some(handle) = handle else { return Ok(()) };
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("writing {what} to ffmpeg"))
        }
        Err(_) => bail!("{what} writer thread panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration the argument builders can be exercised with. Small on purpose: these
    /// tests are about the argument *list*, not about encoding anything.
    fn args_cfg(fps: u32, segment_ms: u64) -> EncodeConfig {
        let mut cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            1280,
            720,
            fps,
            ".".into(),
            segment_ms,
        );
        cfg.bitrate_kbps = 20_000;
        cfg
    }

    /// The rate in the child's argument list is `EncodeConfig::fps`, and nothing else. This
    /// is the number the pacer is built from as well (see `Encoder::input_fps`): one value,
    /// two consumers, which is the fix for issues #1/#2 — so a change that makes ffmpeg
    /// declare a different rate from the one the pipeline paces to has to fail here.
    #[test]
    fn the_declared_rate_is_the_configurations_fps_and_it_is_declared_as_framerate() {
        for fps in [10u32, 24, 30, 60] {
            let cfg = args_cfg(fps, 1000);
            let args = video_input_args(&cfg);
            let at = |flag: &str| {
                let i = args
                    .iter()
                    .position(|a| a == flag)
                    .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
                args.get(i + 1).cloned().unwrap_or_default()
            };
            assert_eq!(at("-framerate"), fps.to_string(), "the declared rate: {args:?}");
            assert_eq!(at("-s"), "1280x720", "the rawvideo pipe is the SOURCE size: {args:?}");
            assert!(
                !args.iter().any(|a| a == "-r"),
                "`-r` before `-i` re-stamps every frame onto a rigid grid and undoes the \
                 arrival timestamps this option records (measured; see the module comment): \
                 {args:?}"
            );
            assert_eq!(at("-use_wallclock_as_timestamps"), "1", "{args:?}");
        }
    }

    /// The hybrid's argument pair: what must be there, and — more importantly — what must not.
    ///
    /// The absences carry the lessons this project paid for. `-force_key_frames` and `-g` cannot
    /// apply to a stream ffmpeg is only copying, so a version that included them would look right
    /// and impose nothing: the segment boundaries the replay path depends on would silently come
    /// from nowhere. `-r` and `-framerate` on the input would re-grid it, which is the landmine
    /// issue #2 recorded. And `-vf scale` would mean decoding, which would mean the pixels, which is
    /// the entire cost this path exists to remove.
    #[test]
    fn the_bitstream_args_copy_the_video_and_impose_nothing_on_it() {
        let input = video_input_args_bitstream();
        let got: Vec<&str> = input.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            ["-f", "h264", "-use_wallclock_as_timestamps", "1", "-i", "pipe:0"],
            "the input is an H.264 elementary stream, stamped on arrival"
        );
        for forbidden in ["-r", "-framerate"] {
            assert!(
                !input.iter().any(|a| a == forbidden),
                "{forbidden} would re-grid the input, which is the bug issue #2 was: {input:?}"
            );
        }

        let output = video_output_args_bitstream();
        let got: Vec<&str> = output.iter().map(String::as_str).collect();
        assert_eq!(got, ["-c:v", "copy"], "the video is copied, not encoded again");
        for forbidden in ["-force_key_frames", "-g", "-b:v", "-fps_mode", "-vf"] {
            assert!(
                !output.iter().any(|a| a == forbidden),
                "{forbidden} cannot apply to a copied stream, and including it would impose \
                 nothing while looking correct: {output:?}"
            );
        }
    }

    /// A bitstream input produces the hybrid's pair, and a scaled one is refused.
    ///
    /// The absences carry as much as the presences. `-s` and `-framerate` describe a pipe of pixels
    /// and must be gone; `-b:v` and `-force_key_frames` describe work ffmpeg is no longer doing. And
    /// the refusal at the end is the point of the guard: a scaled output under this mode is a
    /// contradiction — scaling means decoding, decoding means the pixels this mode exists to avoid —
    /// so it has to be an error rather than a silently different geometry in the user's recording.
    ///
    /// What must *not* change is as important: the audio input and the output packaging are the same
    /// arguments, because that is the whole argument for doing it this way.
    #[test]
    fn a_bitstream_input_copies_the_video_and_refuses_to_scale_it() {
        let mut cfg = args_cfg(30, 1000);
        cfg.video = VideoInput::EncodedBitstream;

        let args = ffmpeg_args(&cfg, "tcp://127.0.0.1:1", None);
        let text: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(text.windows(2).any(|w| w == ["-f", "h264"]), "{text:?}");
        assert!(
            text.windows(2).any(|w| w == ["-use_wallclock_as_timestamps", "1"]),
            "an elementary stream carries no timestamps of its own: {text:?}"
        );
        assert!(text.windows(2).any(|w| w == ["-c:v", "copy"]), "{text:?}");
        for forbidden in ["-s", "-framerate", "-b:v", "-force_key_frames"] {
            assert!(
                !text.iter().any(|a| a == forbidden),
                "{forbidden} belongs to the rawvideo path and must not survive into this one: \
                 {text:?}"
            );
        }
        assert!(text.windows(2).any(|w| w == ["-c:a", "aac"]), "audio is unchanged: {text:?}");
        assert!(text.windows(2).any(|w| w == ["-f", "segment"]), "so is the output: {text:?}");

        // A scaled output is refused, and refused before anything is created or launched — the
        // binary named here does not exist, which is the point.
        cfg.source_size = (1280, 720);
        cfg.output_size = (640, 360);
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        // Matched rather than `expect_err`, which would need `Debug` on an encoder that owns a
        // child process and has no business deriving it.
        let refused = match FfmpegEncoder::spawn(&bin, &cfg) {
            Ok(_) => panic!("a scaled bitstream must be refused, and this one was not"),
            Err(err) => err,
        };
        assert!(
            refused.to_string().contains("cannot be scaled"),
            "the refusal must name the reason rather than fail later: {refused}"
        );
    }

    /// The scale filter is added only when the output size differs, and the GOP follows the
    /// declared rate — both of which the throughput probe inherits, because it builds its
    /// argument list from these same two functions.
    #[test]
    fn the_output_arguments_scale_only_when_asked_and_size_the_gop_from_the_rate() {
        let same = args_cfg(30, 1000);
        assert!(
            !video_output_args(&same).iter().any(|a| a == "-vf"),
            "equal sizes must not add a pointless scale pass"
        );

        let mut scaled = args_cfg(30, 1000);
        scaled.output_size = (1920, 1080);
        let args = video_output_args(&scaled);
        let vf = args.iter().position(|a| a == "-vf").expect("a scale filter");
        assert_eq!(args[vf + 1], "scale=1920:1080");
        assert!(args.contains(&"-c:v".to_string()));

        // One keyframe per segment: at 30fps and 1s segments the GOP is 30, at 2s it is 60.
        let gop = |cfg: &EncodeConfig| {
            let args = video_output_args(cfg);
            let i = args.iter().position(|a| a == "-g").expect("-g");
            args[i + 1].parse::<u32>().expect("a GOP number")
        };
        assert_eq!(gop(&args_cfg(30, 1000)), 30);
        assert_eq!(gop(&args_cfg(30, 2000)), 60);
        assert_eq!(gop(&args_cfg(24, 1000)), 24);
    }

    /// The frame-rate conversion is off, on every arm.
    ///
    /// This is the option that makes the media timeline the wall clock rather than a declared
    /// grid (see [`video_output_args`] and the module comment): with the default constant-rate
    /// conversion, ffmpeg *invents* frames to fill a declared `1/R` grid, so media time
    /// advances at (frames the encoder can emit) ÷ R instead of tracking real time. A change
    /// that removes it, renames it back to a spelling the sidecar does not know, or moves it
    /// before the input (where it would be ignored) has to fail here.
    #[test]
    fn frames_are_passed_through_and_never_resampled_onto_a_declared_grid() {
        for (fps, segment_ms) in [(30u32, 1000u64), (60, 500), (120, 1000)] {
            let cfg = args_cfg(fps, segment_ms);
            let output = video_output_args(&cfg);
            let i = output
                .iter()
                .position(|a| a == "-fps_mode")
                .unwrap_or_else(|| panic!("-fps_mode missing from {output:?}"));
            assert_eq!(output[i + 1], "passthrough", "at {fps}fps: {output:?}");
            assert!(
                !output.iter().any(|a| a == "-r" || a == "-fpsmax"),
                "a frame-rate override re-introduces the grid this option removes: {output:?}"
            );
        }
    }

    /// The argument list as the os-string vector `Command::args` receives it, for
    /// comparison with a written-out expectation. Every element is ASCII here; only the
    /// segment pattern is a path, and it is derived the same way on both sides.
    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// The microphone's contract, in the direction that must never regress: with
    /// `mic_audio: None` the child is spawned with **exactly** the arguments it was spawned
    /// with before the second audio input existed.
    ///
    /// The baseline below is transcribed element for element from `FfmpegEncoder::spawn` as
    /// it stood before this change — written out in full, deliberately, because an
    /// expectation built from the builders under test would assert nothing at all. Adding,
    /// removing, reordering or re-spelling anything on the single-audio path fails here, and
    /// that is the point: an argument list that only *looks* equivalent is how the
    /// `-start_number`/`-segment_start_number` and `-r`/`-framerate` mistakes happened (see
    /// the comments in [`ffmpeg_args`]).
    #[test]
    fn the_argument_list_without_a_microphone_is_byte_identical_to_the_pre_change_list() {
        let cfg = args_cfg(30, 1_000);

        let got = strings(&ffmpeg_args(&cfg, "tcp://127.0.0.1:41234", None));
        let mut expected: Vec<String> = vec![
            "-hide_banner", "-loglevel", "error", "-nostdin",
            // Input 0: raw BGRA video on stdin, at the declared rate, timed from arrival.
            "-f", "rawvideo", "-pix_fmt", "bgra", "-s", "1280x720", "-framerate", "30",
            "-use_wallclock_as_timestamps", "1", "-i", "pipe:0",
            // Input 1: raw s16le PCM over loopback TCP.
            "-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "tcp://127.0.0.1:41234",
            // The video encoder: the passthrough frame-rate mode, the codec, its bitrate,
            // its GOP and its forced keyframes.
            "-fps_mode", "passthrough", "-c:v", "libx264", "-b:v", "20000k", "-g", "30",
            "-force_key_frames", "expr:gte(t,n_forced*1)",
            // The audio codec, then the segment muxer and its numbering.
            "-c:a", "aac", "-b:a", "128k", "-f", "segment", "-segment_time", "1",
            "-segment_format", "mp4", "-reset_timestamps", "1", "-segment_start_number", "0",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        // The one element that is a real path, built the way the encoder builds it, so the
        // comparison does not depend on the platform's path separator.
        expected.push(cfg.scratch_dir.join("seg-%06d.mp4").to_string_lossy().into_owned());

        assert_eq!(
            got, expected,
            "the single-audio argument list must be byte-identical to the pre-change list. \
             Note what is absent: no `-map` and no `-metadata` — with one audio input \
             ffmpeg's default stream selection already picks exactly input 0's video and \
             input 1's audio, which is why the microphone is the only thing that adds them"
        );
    }

    /// The other direction: enabling the microphone adds *its own input* and the
    /// mapping/tagging, and moves nothing else.
    ///
    /// The frozen shapes are all visible here: input 2 is `tcp://127.0.0.1:{mic_audio_port}`
    /// declared from the caller's `MicAudioSpec`, and the output carries
    /// `-map 0:v -map 1:a -map 2:a` with `-metadata:s:a:0 title=Game Audio` and
    /// `-metadata:s:a:1 title=Microphone`. The video arguments — including the
    /// `-fps_mode passthrough` that keeps the media timeline on the wall clock — are the
    /// same elements in the same order as the single-audio list, because the second audio
    /// input is declared between them and changes nothing about them.
    #[test]
    fn enabling_the_microphone_adds_its_own_input_its_mapping_and_its_tags() {
        let mut cfg = args_cfg(30, 1_000);
        // Deliberately NOT the default spec. The sample rate and the channel count are
        // numbers the child is *told* (`-ar` / `-ac`), and a spec that never reaches them
        // would put the microphone track on a timeline of the wrong length.
        cfg.mic_audio = Some(crate::MicAudioSpec {
            sample_rate: 44_100,
            channels: 1,
            sample_format: SampleFormat::S16Le,
        });

        let got =
            strings(&ffmpeg_args(&cfg, "tcp://127.0.0.1:41234", Some("tcp://127.0.0.1:43210")));
        let mut expected: Vec<String> = vec![
            "-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "rawvideo", "-pix_fmt", "bgra", "-s", "1280x720", "-framerate", "30",
            "-use_wallclock_as_timestamps", "1", "-i", "pipe:0",
            "-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "tcp://127.0.0.1:41234",
            // Input 2, from the configuration's spec.
            "-f", "s16le", "-ar", "44100", "-ac", "1", "-i", "tcp://127.0.0.1:43210",
            "-fps_mode", "passthrough", "-c:v", "libx264", "-b:v", "20000k", "-g", "30",
            "-force_key_frames", "expr:gte(t,n_forced*1)",
            "-c:a", "aac", "-b:a", "128k",
            // What a second audio input makes necessary, and the only two things this
            // change adds: both audio streams are mapped explicitly (without maps ffmpeg
            // would pick ONE of them) and each is tagged, because a two-track container
            // whose tracks cannot be told apart is not the feature.
            "-map", "0:v", "-map", "1:a", "-map", "2:a",
            "-metadata:s:a:0", "title=Game Audio",
            "-metadata:s:a:1", "title=Microphone",
            "-f", "segment", "-segment_time", "1", "-segment_format", "mp4",
            "-reset_timestamps", "1", "-segment_start_number", "0",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        expected.push(cfg.scratch_dir.join("seg-%06d.mp4").to_string_lossy().into_owned());

        assert_eq!(got, expected, "the microphone adds its input, its mapping and its tags");
    }

    #[test]
    fn a_payload_that_fits_is_queued_and_not_counted_as_dropped() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 7, &dropped, "video").expect("a free slot must accept the payload");

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(rx.try_recv().expect("the payload is queued"), 7);
    }

    #[test]
    fn a_full_queue_drops_the_payload_and_counts_it_instead_of_blocking_or_failing() {
        // Capacity 1, and nothing drains it: this is the "encoder cannot keep up" state.
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 1, &dropped, "video").expect("a free slot accepts the payload");
        // These must return immediately and successfully — `send` here would block the
        // capture loop forever, and an error would stop the recording.
        enqueue_or_drop(&tx, 2, &dropped, "video").expect("a full queue drops, it does not fail");
        enqueue_or_drop(&tx, 3, &dropped, "video").expect("a full queue drops, it does not fail");

        assert_eq!(dropped.load(Ordering::Relaxed), 2, "both refused payloads are counted");
        assert_eq!(
            rx.try_recv().expect("the queued payload is still there"),
            1,
            "the queued payload is untouched"
        );
        assert!(rx.try_recv().is_err(), "the dropped payloads were not smuggled in behind it");
    }

    #[test]
    fn a_stopped_writer_thread_is_an_error_not_a_drop() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);
        drop(rx);

        let err = enqueue_or_drop(&tx, 1, &dropped, "video")
            .expect_err("nothing can be encoded once the writer thread is gone");
        assert!(
            err.to_string().contains("video writer thread has stopped"),
            "the error must name the dead writer: {err}"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0, "a failure is not a drop");
    }

    /// A pipe can only be read once, and two reports want ffmpeg's words: the writer
    /// failure that explains a dead encoder, and the failure [`Encoder::finish`] raises
    /// afterwards. The second one must not be left with nothing — that would move the
    /// unexplained failure from one place to another.
    #[cfg(unix)]
    #[test]
    fn stderr_is_remembered_so_a_second_report_is_not_left_empty() {
        let child = Command::new("/bin/sh")
            .args(["-c", "echo 'DLL amfrt64.dll failed to open' >&2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The child is written by hand here because `spawn` needs a real ffmpeg and an
        // audio listener; this is the smallest thing that has a child with stderr.
        let mut encoder = FfmpegEncoder {
            child,
            output_stream: None,
            video_tx: None,
            audio_tx: None,
            mic_tx: None,
            video_writer: None,
            audio_writer: None,
            mic_writer: None,
            encoder_name: "h264_amf",
            source_size: (320, 240),
            input_fps: 30,
            mic_port: None,
            mic_spec: None,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            dropped_mic_audio: AtomicU64::new(0),
            drained_stderr: None,
        };
        encoder.child.wait().unwrap();

        let first = encoder.drain_stderr(Duration::from_secs(1));
        assert!(first.contains("DLL amfrt64.dll failed to open"), "got: {first:?}");
        let second = encoder.drain_stderr(Duration::from_secs(1));
        assert_eq!(second, first, "the second report gets the same words, not nothing");
    }

    /// The message a Windows capture produced was `Error: video writer thread has stopped`
    /// while the real cause — an `h264_amf` that could not initialise — sat in ffmpeg's
    /// stderr. A dead writer must now carry the child's own exit status and words.
    ///
    /// The stand-in child is a script that fails the way a hardware encoder does on a
    /// machine with no vendor runtime: it says why on stderr and exits non-zero. No GPU is
    /// needed, and the path under test — a pump thread noticing the broken pipe, then the
    /// submitter asking the child what happened — is the one the real encoder uses.
    #[cfg(unix)]
    /// A frame with no pixels is refused rather than written as a zero-length frame.
    ///
    /// This guard is what makes the zero-copy plumbing safe to land *ahead* of the encoder that
    /// consumes textures. A frame whose pixels stayed in VRAM has an empty `data` by design, and
    /// this encoder writes `data` to ffmpeg's stdin — so accepting one would hand the child a
    /// zero-length rawvideo frame, which it reads as the next frame's leading bytes. The damage
    /// would then appear seconds later as a stream desync pointing at nothing in this function.
    #[test]
    fn a_frame_without_pixels_is_refused_rather_than_written_as_nothing() {
        use localplay_capture::{Frame, PixelFormat};
        use crate::VideoCodec;

        let dir = tempfile::tempdir().unwrap();
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");

        let pixel_less = Frame {
            data: Vec::new(),
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        let err = encoder
            .submit_video(pixel_less)
            .expect_err("a frame with no pixels must be refused");
        assert!(
            err.to_string().contains("not in CPU memory"),
            "the refusal must name the reason rather than surfacing later as corruption: {err}"
        );

        // And a frame with pixels still goes through, so the guard refuses a shape rather than
        // quietly refusing work.
        let honest = Frame {
            data: vec![0u8; 64 * 64 * 4],
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        encoder.submit_video(honest).expect("a frame with pixels is accepted");
        encoder.finish().expect("flush the encoder");
    }

    #[test]
    fn a_dead_writer_reports_ffmpegs_exit_status_and_stderr() {
        use crate::VideoCodec;
        use localplay_capture::{Frame, PixelFormat};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        std::fs::write(&stub, "#!/bin/sh\necho 'DLL amfrt64.dll failed to open' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries { ffmpeg: stub.clone(), ffprobe: stub };
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        // Spawning succeeds: the child starts and then fails, which is exactly the shape
        // of an encoder that cannot initialise.
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).unwrap();

        // The channel only disconnects once the pump thread *tries* to write into the
        // dead child and fails, so submit until that happens; each attempt is an
        // independent frame the encoder may accept first.
        let frame = || Frame {
            data: vec![0u8; 64 * 64 * 4],
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        let mut failure = None;
        for _ in 0..50 {
            match encoder.submit_video(frame()) {
                Ok(()) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }

        let err = failure.expect("a payload cannot be encoded once the writer thread is gone");
        assert!(err.contains("video writer thread has stopped"), "the symptom survives: {err}");
        assert!(
            err.contains("DLL amfrt64.dll failed to open"),
            "ffmpeg's own words are the point, and they were only in its stderr: {err}"
        );
        assert!(err.contains("exit status"), "and so is its exit status: {err}");
    }

    /// The trap this codebase has been bitten by twice (`docs/platform-traps.md`, trap 1):
    /// on macOS and the other BSD-derived platforms the socket `accept()` returns inherits
    /// the **listener's** `O_NONBLOCK`, so a write that would have waited for ffmpeg to
    /// drain the socket fails immediately with `WouldBlock` (`EAGAIN`, os error 35) the
    /// moment the socket buffers fill. It shows up as a flake — "the pump fails only when it
    /// happens to get ahead of ffmpeg" — so a test that hopes to hit the race proves
    /// nothing; this one makes the state certain instead.
    ///
    /// [`accept_within`] forces the accepted socket back to blocking mode for exactly this
    /// reason, and the microphone input is a **second copy of that accept path**
    /// (`pump_audio` calls this and nothing else before writing), so it needs the same line.
    /// The client here is a real ffmpeg on the same `s16le` transport the encoder declares,
    /// paced to real time with `-re` and given a small receive buffer, so it cannot swallow
    /// a megabyte whole: the writer has to *wait* for it.
    ///
    /// The two behaviours are told apart by the socket's own write timeout, which is the
    /// discriminator `docs/platform-traps.md` names: a blocking socket honours it (the write
    /// waits out the timeout and then reports `WouldBlock`), a non-blocking socket ignores it
    /// and reports `WouldBlock` immediately. So the assertion is "the large write did not
    /// fail at once", which is exactly "the accepted socket is blocking".
    ///
    /// The client is ffmpeg rather than a `TcpStream::connect`: the tree's outbound sockets
    /// are enumerated by `crates/events/tests/no_egress.rs` and a new one belongs in that
    /// list deliberately. ffmpeg dialling a listener we bound is the production shape anyway.
    #[cfg(feature = "test-encoders")]
    #[test]
    fn a_large_block_written_to_the_microphone_input_waits_instead_of_failing_with_eagain() {
        /// How long a write is willing to wait for the reader. The assertion is about
        /// whether this is *honoured*, so the number only has to be comfortably larger than
        /// the few milliseconds a non-blocking socket needs to give up.
        const WRITE_PATIENCE: Duration = Duration::from_millis(300);

        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let port = listener.local_addr().expect("the listener's address").port();

        let mut client = Command::new(&bin.ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-nostdin",
                // Real-time pacing: the reader takes the transport at the rate the PCM
                // represents, so it cannot drain a burst faster than the socket buffers
                // allow — which is the state that used to make `write_all` fail on macOS.
                "-re",
                // A small receive buffer keeps the burst decisively larger than the
                // socket's own capacity.
                "-recv_buffer_size", "32768",
                "-f", "s16le", "-ar", "48000", "-ac", "2",
                "-i", &format!("tcp://127.0.0.1:{port}"),
                "-f", "null", "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ffmpeg can act as the client of a loopback listener");

        // The microphone input's own accept, with the microphone's own label.
        let mut stream = accept_within(&listener, port, AUDIO_CONNECT_TIMEOUT, MICROPHONE_LABEL)
            .expect("ffmpeg dials the microphone listener");
        stream
            .set_write_timeout(Some(WRITE_PATIENCE))
            .expect("the accepted socket takes a write timeout");
        // Four megabytes — 22 s of 48kHz stereo s16le — written immediately after connect,
        // like a pump that just received a burst, or that is catching up after a stall. It
        // is several times what a real-time-paced reader absorbs before it starts pacing
        // (measured on this host: ffmpeg takes ~1 MiB at once and then reads at the PCM's
        // own rate), so the write has to wait rather than being swallowed whole.
        let burst = vec![0u8; 4 * 1024 * 1024];
        let started = Instant::now();
        let outcome = stream.write_all(&burst);
        let waited = started.elapsed();

        let _ = client.kill();
        let _ = client.wait();

        match outcome {
            // A reader that swallowed a megabyte inside the write timeout would mean
            // nothing ever had to wait, so this run would be inconclusive rather than
            // wrong: a non-blocking socket cannot reach this arm (it fails at once), so
            // the regression could not hide here.
            Ok(()) => eprintln!(
                "note: the client drained the whole {}-byte burst within {WRITE_PATIENCE:?}; \
                 the write never had to wait",
                burst.len()
            ),
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock,
                    "a full socket buffer is not a write failure: {e}"
                );
                assert!(
                    waited >= WRITE_PATIENCE - Duration::from_millis(50),
                    "the write gave up after {waited:?} instead of waiting out its \
                     {WRITE_PATIENCE:?} timeout: the accepted socket is NON-BLOCKING, i.e. it \
                     inherited O_NONBLOCK from the listener — docs/platform-traps.md, trap 1 \
                     (this is the macOS/BSD inheritance `set_nonblocking(false)` exists for)"
                );
            }
        }
    }
}
