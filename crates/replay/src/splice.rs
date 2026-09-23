//! Turning a segment window into a single lossless clip file.

use crate::window::SegmentWindow;
use anyhow::{bail, Context, Result};
use localplay_media::{edit, FfmpegBinaries};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub encoder: String,
}

pub struct ClipSplicer;

impl ClipSplicer {
    /// Concatenate whole segments with `-c copy`.
    ///
    /// No leading-segment trim: segment boundaries are keyframes, so `-ss`/`-c copy`
    /// would resolve to the same frame and change nothing (see plan refinements).
    pub fn splice(
        bin: &FfmpegBinaries,
        window: &SegmentWindow,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        if window.segments.is_empty() {
            bail!("cannot splice an empty segment window");
        }

        let list = out.with_extension("concat.txt");
        {
            let mut f = std::fs::File::create(&list)
                .with_context(|| format!("creating {}", list.display()))?;
            for seg in &window.segments {
                // The concat demuxer needs escaped, absolute-ish paths.
                let p = std::fs::canonicalize(&seg.file)
                    .with_context(|| format!("resolving {}", seg.file.display()))?;
                writeln!(f, "file '{}'", p.display().to_string().replace('\'', "'\\''"))?;
            }
        }

        edit::concat_lossless(bin, &list, out)?;
        let _ = std::fs::remove_file(&list);

        let info = localplay_media::MediaInfo::probe(bin, out)?;

        // A/V offset **within the produced clip** (spec §11 criterion 8). This is the
        // observable that matters: it compares the audio and video stream timelines of
        // the muxed file. It is NOT the QPC-clock divergence between the two live
        // capture sources, which would require instrumenting the capture path itself.
        let stem = out
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip");
        match info.av_drift() {
            Some(d) => tracing::info!(
                "clip {stem}: video {}ms audio {}ms drift {}ms",
                d.video_ms,
                d.audio_ms,
                d.delta_ms
            ),
            None => tracing::warn!(
                "clip {stem}: A/V drift unavailable — ffprobe reported no per-stream \
                 duration (video {}ms, audio {}ms); the streams are present but the \
                 offset cannot be computed honestly",
                opt_ms(info.video.as_ref().and_then(|v| v.duration_ms)),
                opt_ms(info.audio.as_ref().and_then(|a| a.duration_ms)),
            ),
        }

        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }
}

/// Render an optional millisecond count for the log line, saying "unavailable"
/// rather than pretending a missing value is 0.
fn opt_ms(ms: Option<u64>) -> String {
    ms.map(|m| m.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}
