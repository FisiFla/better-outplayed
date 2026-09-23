//! `ffprobe` JSON into a typed `MediaInfo`.

use crate::binaries::{run_with_stdin, run_with_timeout, FfmpegBinaries};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bit_rate: Option<u64>,
    /// Per-stream duration (`stream.duration`), in ms. `None` when ffprobe does not
    /// report one for this stream (some containers omit it).
    pub duration_ms: Option<u64>,
    /// Per-stream start time (`stream.start_time`), in ms. May be negative for a
    /// stream with an edit list. `None` when ffprobe does not report one.
    pub start_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStream {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Per-stream duration (`stream.duration`), in ms. `None` when absent.
    pub duration_ms: Option<u64>,
    /// Per-stream start time (`stream.start_time`), in ms. `None` when absent.
    pub start_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub video: Option<VideoStream>,
    pub audio: Option<AudioStream>,
}

/// A/V offset observed **within one produced file**.
///
/// This compares the audio stream's timeline to the video stream's timeline in the
/// muxed clip — the observable that matters for the criteria. It is deliberately
/// *not* a measurement of clock divergence between the two live capture sources:
/// that would require instrumenting the capture path (QPC vs the audio device clock)
/// and is a different quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvDrift {
    /// Video stream duration, ms.
    pub video_ms: u64,
    /// Audio stream duration, ms.
    pub audio_ms: u64,
    /// Video stream start time when ffprobe reported one.
    pub video_start_ms: Option<i64>,
    /// Audio stream start time when ffprobe reported one.
    pub audio_start_ms: Option<i64>,
    /// `video_end - audio_end` (end = start + duration; start defaults to 0 when
    /// ffprobe does not report one). Negative when the audio stream runs **longer**
    /// than the video stream.
    pub delta_ms: i64,
}

#[derive(Deserialize)]
struct RawProbe {
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: String,
    codec_name: String,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
    /// `stream.duration` as a decimal string of seconds, when present.
    duration: Option<String>,
    /// `stream.start_time` as a decimal string of seconds, when present.
    start_time: Option<String>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
    size: Option<String>,
}

impl MediaInfo {
    pub fn from_ffprobe_json(json: &str) -> Result<Self> {
        let raw: RawProbe = serde_json::from_str(json).context("parsing ffprobe json")?;
        if raw.streams.is_empty() {
            bail!("ffprobe reported no streams");
        }

        let video = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "video")
            .map(|s| VideoStream {
                codec: s.codec_name.clone(),
                width: s.width.unwrap_or(0),
                height: s.height.unwrap_or(0),
                bit_rate: s.bit_rate.as_deref().and_then(|v| v.parse().ok()),
                duration_ms: seconds_to_ms_opt(s.duration.as_deref()),
                start_ms: seconds_to_ms_i64_opt(s.start_time.as_deref()),
            });

        let audio = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .map(|s| AudioStream {
                codec: s.codec_name.clone(),
                sample_rate: s.sample_rate.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
                channels: s.channels.unwrap_or(0),
                duration_ms: seconds_to_ms_opt(s.duration.as_deref()),
                start_ms: seconds_to_ms_i64_opt(s.start_time.as_deref()),
            });

        Ok(Self {
            duration_ms: seconds_to_ms(raw.format.duration.as_deref()),
            size_bytes: raw.format.size.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
            video,
            audio,
        })
    }

    /// Run `ffprobe` against a file on disk.
    pub fn probe(bin: &FfmpegBinaries, path: &Path) -> Result<Self> {
        let mut cmd = Command::new(&bin.ffprobe);
        cmd.args([
            "-v", "error",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path);
        let out = run_with_timeout(cmd, PROBE_TIMEOUT)?;
        if !out.status.success() {
            bail!(
                "ffprobe failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Self::from_ffprobe_json(&String::from_utf8_lossy(&out.stdout))
    }

    /// The A/V offset of **this file**: video end minus audio end, in ms.
    ///
    /// Returns `None` unless both an audio and a video stream are present *and*
    /// ffprobe reported a per-stream duration for both — there is nothing honest to
    /// compute otherwise, and the caller should log what is available instead.
    ///
    /// End = `start_ms` (default `0` when unreported) + `duration_ms`. `delta_ms` is
    /// `video_end - audio_end`, so it goes **negative when the audio runs longer than
    /// the video**.
    ///
    /// This measures drift *within the produced clip*. It is not the divergence
    /// between the live video and audio capture clocks.
    pub fn av_drift(&self) -> Option<AvDrift> {
        let video = self.video.as_ref()?;
        let audio = self.audio.as_ref()?;
        let video_dur = video.duration_ms?;
        let audio_dur = audio.duration_ms?;
        let video_end = video.start_ms.unwrap_or(0) + video_dur as i64;
        let audio_end = audio.start_ms.unwrap_or(0) + audio_dur as i64;
        Some(AvDrift {
            video_ms: video_dur,
            audio_ms: audio_dur,
            video_start_ms: video.start_ms,
            audio_start_ms: audio.start_ms,
            delta_ms: video_end - audio_end,
        })
    }
}

/// Encode exactly one frame with `encoder` and throw the output away.
///
/// `Ok` means this machine can actually drive that encoder; `Err` carries **ffmpeg's own
/// words** about why it cannot, because that is the only actionable diagnosis there is (a
/// missing `amfrt64.dll` names itself).
///
/// This exists because `ffmpeg -encoders` cannot answer the question. That list is what
/// ffmpeg was *compiled* with, not what its runtime can initialise: measured on an RTX 3090
/// box with no AMD hardware or driver at all, ffmpeg advertised `h264_amf` and then died
/// with `DLL amfrt64.dll failed to open` the moment it was asked to open the encoder. The
/// design spec called for this smoke test alongside the encoder list (§5.2, and §13 lists
/// the encoder list alone as a risk) and it is the difference between failing at startup
/// and failing mid-capture.
///
/// One 320x240 BGRA frame (307 200 bytes) goes in over `pipe:0` as rawvideo, exactly as the
/// live capture feeds the real encoder, and the output goes to the null muxer. There is
/// deliberately no `-f lavfi` test source: a strip-down ffmpeg build may not carry those
/// filters, and the point is to exercise the encoder, not a filter graph. The child's exit
/// status is the answer — a frame ffmpeg accepted is what proves the encoder opened.
///
/// Nothing here touches the screen or synthesises input: it is a synthetic frame, not a
/// capture.
pub fn smoke_test_encoder(bin: &FfmpegBinaries, encoder: &str) -> Result<(), String> {
    let size = format!("{}x{}", SMOKE_SIZE.0, SMOKE_SIZE.1);
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args([
        "-hide_banner",
        // Keeps ffmpeg from reading the probe's own stdin for interactive commands; the
        // frame still arrives on the `pipe:0` input below.
        "-nostdin",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgra",
        "-s",
        size.as_str(),
        "-i",
        "pipe:0",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ]);
    let frame = vec![0u8; (SMOKE_SIZE.0 * SMOKE_SIZE.1 * 4) as usize];
    let out = run_with_stdin(cmd, frame, SMOKE_TIMEOUT).map_err(|e| format!("{e:#}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(reason_from(&String::from_utf8_lossy(&out.stderr)))
}

/// ffmpeg's explanation of a failed smoke test, condensed onto one line.
///
/// Shapes this handles, both measured against ffmpeg 9.0.2:
///
/// ```text
/// Input #0, rawvideo, from 'pipe:0':                     <- filtered: description
///   Stream #0:0: Video: rawvideo ..., bgra, 320x240       <- filtered: description
/// [h264_amf @ 0x55…] DLL amfrt64.dll failed to open      <- THE reason
/// [h264_amf @ 0x55…] Error initializing an external …    <- second reason
/// Error while opening encoder for output stream #0:0 …   <- ffmpeg's own summary
/// ```
///
/// The description lines are dropped (they say nothing about the failure), the reasons are
/// kept first because that is where the missing DLL or driver is *named*, and the closing
/// summary is kept because it is ffmpeg's statement of what went wrong overall. Middle
/// lines are usually a restatement of the first two, so only the first two and the last are
/// used, and the result is capped so it cannot become a wall of text in a log or a table.
fn reason_from(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_ffmpeg_chatter(l))
        .collect();
    let reason = match lines.as_slice() {
        [] => "ffmpeg said nothing on stderr".to_string(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} | {second}"),
        [first, second, .., last] => format!("{first} | {second} | {last}"),
    };
    if reason.chars().count() > REASON_CAP {
        let kept: String = reason.chars().take(REASON_CAP).collect();
        return format!("{kept}…");
    }
    reason
}

/// ffmpeg's non-diagnostic output: the description of the input and output it is about to
/// build, its stream mapping, and the progress line. None of it says why an encoder failed,
/// and all of it would otherwise be read as "the reason" because it comes first.
///
/// `Stream #` and `Duration:` are matched after trimming, so the two-space indentation
/// ffmpeg uses inside a stream block does not defeat the match.
fn is_ffmpeg_chatter(line: &str) -> bool {
    const CHATTER: [&str; 9] = [
        "Input #",
        "Output #",
        "Stream mapping",
        "Metadata:",
        "Duration:",
        "Stream #",
        "Press [q]",
        "frame=",
        "size=",
    ];
    CHATTER.iter().any(|prefix| line.starts_with(prefix))
}

/// The frame a smoke test encodes: 320x240 BGRA, i.e. 307 200 bytes.
///
/// The size is **not** free to shrink. Hardware encoders have vendor minimum sizes, and a
/// probe that trips one reports a working encoder as unusable — which is the opposite of
/// what this test is for:
///
/// - **NVENC** rejects anything under 145x145 *at the driver*, with "Frame Dimension less
///   than the minimum supported value" (ffmpeg trac #9251: 144x144 fails, 145x145 works). A
///   64x64 probe would therefore have called a perfectly good RTX 3090 unusable.
/// - **QSV** has a minimum around 128x96.
/// - **AMF** documents 64x64 for H.264 but 192x128 for HEVC, and one probe size has to
///   serve both codecs.
///
/// 320x240 clears every one of those, is 16-pixel aligned in both directions, and is one
/// frame of 300 KiB: large enough to be a real encode, small enough that feeding it costs
/// nothing.
const SMOKE_SIZE: (u32, u32) = (320, 240);

/// How long a smoke test may take before the encoder is declared unusable. An encoder that
/// needs the GPU runtime and cannot get it fails in milliseconds; this is a ceiling on a
/// wedged child, not a budget an honest one needs.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the length of a returned reason, in characters.
const REASON_CAP: usize = 400;

/// ffprobe reports duration as a decimal string of seconds.
fn seconds_to_ms(seconds: Option<&str>) -> u64 {
    seconds_to_ms_opt(seconds).unwrap_or(0)
}

/// Like [`seconds_to_ms`], but preserves "ffprobe did not report this" as `None`
/// instead of collapsing it to `0`.
fn seconds_to_ms_opt(seconds: Option<&str>) -> Option<u64> {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as u64)
}

/// Parse a decimal-seconds string (possibly negative, e.g. an edit-list start time)
/// into whole milliseconds.
fn seconds_to_ms_i64_opt(seconds: Option<&str>) -> Option<i64> {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_combined_audio_video_probe() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240,"bit_rate":"500000","duration":"3.033333","start_time":"0.000000"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"duration":"3.050000","start_time":"0.021000"}
          ],
          "format": {"duration":"3.033333","size":"190000"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");

        assert_eq!(info.video.as_ref().map(|v| v.codec.as_str()), Some("h264"));
        assert_eq!(info.video.as_ref().map(|v| (v.width, v.height)), Some((320, 240)));
        assert_eq!(info.video.as_ref().and_then(|v| v.duration_ms), Some(3033));
        assert_eq!(info.video.as_ref().and_then(|v| v.start_ms), Some(0));
        assert_eq!(info.audio.as_ref().map(|a| a.codec.as_str()), Some("aac"));
        assert_eq!(info.audio.as_ref().map(|a| a.channels), Some(2));
        assert_eq!(info.audio.as_ref().and_then(|a| a.duration_ms), Some(3050));
        assert_eq!(info.audio.as_ref().and_then(|a| a.start_ms), Some(21));
        assert_eq!(info.duration_ms, 3033);
        assert_eq!(info.size_bytes, 190000);
    }

    #[test]
    fn rejects_a_probe_with_no_streams() {
        let json = r#"{"streams":[],"format":{"duration":"0.0","size":"0"}}"#;
        assert!(MediaInfo::from_ffprobe_json(json).is_err());
    }

    /// A missing per-stream duration stays `None` rather than silently becoming 0,
    /// so the drift computation can refuse to invent a number.
    #[test]
    fn missing_stream_duration_stays_none() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"duration":"3.0","size":"100"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");
        assert_eq!(info.video.as_ref().and_then(|v| v.duration_ms), None);
        assert_eq!(info.audio.as_ref().and_then(|a| a.duration_ms), None);
    }

    fn stream_durations(video_ms: u64, audio_ms: u64) -> (VideoStream, AudioStream) {
        let video = VideoStream {
            codec: "h264".into(),
            width: 64,
            height: 48,
            bit_rate: None,
            duration_ms: Some(video_ms),
            start_ms: None,
        };
        let audio = AudioStream {
            codec: "aac".into(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: Some(audio_ms),
            start_ms: None,
        };
        (video, audio)
    }

    #[test]
    fn av_drift_is_zero_when_the_streams_are_equal() {
        let (video, audio) = stream_durations(4_000, 4_000);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().expect("both durations present");
        assert_eq!(drift.delta_ms, 0);
        assert_eq!((drift.video_ms, drift.audio_ms), (4_000, 4_000));
    }

    #[test]
    fn av_drift_positive_when_video_outlasts_audio() {
        let (video, audio) = stream_durations(4_000, 3_900);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        assert_eq!(info.av_drift().unwrap().delta_ms, 100);
    }

    /// The negative case asked for by the criteria: audio longer than video makes
    /// the delta negative (the audio runs past the picture).
    #[test]
    fn av_drift_negative_when_audio_outlasts_video() {
        let (video, audio) = stream_durations(3_900, 4_000);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        assert_eq!(info.av_drift().unwrap().delta_ms, -100);
    }

    /// When start times are reported they shift the end timestamps: a video that
    /// starts 50 ms late but has the same duration ends 50 ms later than the audio,
    /// so it reads as the video outlasting the picture's counterpart.
    #[test]
    fn av_drift_accounts_for_stream_start_times() {
        let video = VideoStream {
            codec: "h264".into(),
            width: 64,
            height: 48,
            bit_rate: None,
            duration_ms: Some(4_000),
            start_ms: Some(50),
        };
        let audio = AudioStream {
            codec: "aac".into(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: Some(4_000),
            start_ms: Some(0),
        };
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().unwrap();
        assert_eq!(drift.delta_ms, 50);
        assert_eq!(drift.video_start_ms, Some(50));
        assert_eq!(drift.audio_start_ms, Some(0));
    }

    #[test]
    fn av_drift_is_none_without_both_streams() {
        let json = r#"{
          "streams": [{"codec_type":"video","codec_name":"h264","width":320,"height":240,"duration":"3.0"}],
          "format": {"duration":"3.0","size":"100"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");
        assert!(info.av_drift().is_none(), "no audio stream means no drift");
    }

    /// The probe size is a hardware constraint, not a preference — see [`SMOKE_SIZE`]. This
    /// is the guard against "shrink it, it is only a smoke test": NVENC fails under 145x145
    /// **at the driver**, QSV under roughly 128x96, and AMF's HEVC encoder under 192x128.
    /// A probe below any of those would report working hardware as unusable, which is worse
    /// than not probing at all.
    #[test]
    fn the_smoke_frame_clears_every_vendors_minimum_size() {
        assert!(
            SMOKE_SIZE.0 >= 192 && SMOKE_SIZE.1 >= 145,
            "the smoke frame {}x{} is under a vendor's driver minimum; see the SMOKE_SIZE note",
            SMOKE_SIZE.0,
            SMOKE_SIZE.1
        );
        // 16-pixel alignment, which every one of these encoders also expects.
        assert_eq!(
            (SMOKE_SIZE.0 % 16, SMOKE_SIZE.1 % 16),
            (0, 0),
            "the smoke frame is not 16-pixel aligned"
        );
    }

    /// The real machinery, end to end: a real child, a real frame over `pipe:0`, a real
    /// exit status, and ffmpeg's real words. `libx264` is the encoder this suite's software
    /// path already requires (`crates/encoder/tests/segmenting.rs`), so needing it here
    /// adds no new expectation of the test host.
    ///
    /// This is what a hardware vendor's smoke test rides on: the difference between a
    /// usable and an unusable encoder is only the exit status of exactly this child.
    #[test]
    fn a_working_encoder_encodes_one_frame() {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        smoke_test_encoder(&bin, "libx264").expect("libx264 can encode a 320x240 frame");
    }

    /// The other half: an encoder ffmpeg cannot open must fail *with ffmpeg's reason*, not
    /// with "it did not work". On the Windows box the equivalent reason is
    /// `DLL amfrt64.dll failed to open`, which is only ever in the child's stderr.
    #[test]
    fn an_encoder_that_cannot_open_reports_ffmpegs_own_reason() {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let reason = smoke_test_encoder(&bin, "definitely_not_an_encoder")
            .expect_err("this encoder does not exist");
        assert!(
            reason.contains("definitely_not_an_encoder"),
            "ffmpeg's stderr names what it could not open: {reason}"
        );
        assert!(
            !reason.contains("Input #") && !reason.contains("Stream #"),
            "the input description is not the reason and must not be reported as one: {reason}"
        );
        assert!(!reason.contains('\n'), "a reason is one line: {reason}");
    }

    /// A hanging encoder has to be bounded, not waited out. The stand-in never reads its
    /// stdin, never writes to it, and never exits; the probe must give up on the deadline
    /// instead of wedging startup. The encoded "frame" is deliberately far larger than any
    /// pipe buffer, so the writer thread would block forever if the timeout were not real.
    #[cfg(unix)]
    #[test]
    fn an_encoder_that_hangs_is_bounded_by_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        // `sleep`ing longer than the probe's own deadline, with the frame arriving on a
        // pipe nobody reads.
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        std::fs::write(&stub, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries { ffmpeg: stub.clone(), ffprobe: stub };

        let started = std::time::Instant::now();
        let err = smoke_test_encoder(&bin, "h264_amf").expect_err("a stalled child is not a pass");
        assert!(err.contains("timed out"), "the deadline is named: {err}");
        assert!(
            started.elapsed() < SMOKE_TIMEOUT + Duration::from_secs(5),
            "the probe waited {:?}, which is not bounded by {SMOKE_TIMEOUT:?}",
            started.elapsed()
        );
    }
}
