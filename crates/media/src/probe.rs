//! `ffprobe` JSON into a typed `MediaInfo`.

use crate::binaries::{run_with_timeout, FfmpegBinaries};
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
}
