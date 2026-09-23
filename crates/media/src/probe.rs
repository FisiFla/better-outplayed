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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStream {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub video: Option<VideoStream>,
    pub audio: Option<AudioStream>,
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
            });

        let audio = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .map(|s| AudioStream {
                codec: s.codec_name.clone(),
                sample_rate: s.sample_rate.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
                channels: s.channels.unwrap_or(0),
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
}

/// ffprobe reports duration as a decimal string of seconds.
fn seconds_to_ms(seconds: Option<&str>) -> u64 {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_combined_audio_video_probe() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240,"bit_rate":"500000"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"duration":"3.033333","size":"190000"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");

        assert_eq!(info.video.as_ref().map(|v| v.codec.as_str()), Some("h264"));
        assert_eq!(info.video.as_ref().map(|v| (v.width, v.height)), Some((320, 240)));
        assert_eq!(info.audio.as_ref().map(|a| a.codec.as_str()), Some("aac"));
        assert_eq!(info.audio.as_ref().map(|a| a.channels), Some(2));
        assert_eq!(info.duration_ms, 3033);
        assert_eq!(info.size_bytes, 190000);
    }

    #[test]
    fn rejects_a_probe_with_no_streams() {
        let json = r#"{"streams":[],"format":{"duration":"0.0","size":"0"}}"#;
        assert!(MediaInfo::from_ffprobe_json(json).is_err());
    }
}
