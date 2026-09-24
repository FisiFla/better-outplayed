//! Turning a segment window into a single lossless clip file.

use crate::window::SegmentWindow;
use anyhow::{bail, Result};
use localplay_media::{edit, FfmpegBinaries};
use std::path::{Path, PathBuf};

/// The concat-list path escaping, re-exported so existing importers keep working.
///
/// It lives in `localplay_media::edit` now, beside the concat invocation whose list it
/// renders: the splicer and the recorder's session finalise both build lists, and two
/// copies of the Windows escaping is two places for it to be got wrong.
pub use localplay_media::edit::concat_list_path;

#[derive(Debug, Clone)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub encoder: String,
}

pub struct ClipSplicer;

impl ClipSplicer {
    /// Concatenate whole segments with `-c copy`, keeping every stream.
    ///
    /// No leading-segment trim: segment boundaries are keyframes, so `-ss`/`-c copy`
    /// would resolve to the same frame and change nothing (see plan refinements).
    ///
    /// The layout check and the size-derived budget are shared with the recorder's session
    /// finalise, because this is the same operation. Until they were, this path called
    /// `concat_lossless` with no `-map` and therefore **silently dropped the microphone
    /// track** from any clip cut out of a session recorded with one — exit status 0, no
    /// warning, just a missing stream (see `localplay_media::edit::concat_lossless`).
    pub fn splice(
        bin: &FfmpegBinaries,
        window: &SegmentWindow,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        if window.segments.is_empty() {
            bail!("cannot splice an empty segment window");
        }
        let files: Vec<PathBuf> = window.segments.iter().map(|s| s.file.clone()).collect();

        // Before anything is written: every segment must share one stream layout, or the
        // concatenation drops or truncates a track without ffmpeg reporting it. The layout it
        // returns is also what the concat needs to re-name the audio tracks: a `-c copy` does
        // not carry per-stream metadata, so the names the encoder set are gone by this point.
        let layout = edit::check_concat_layout(bin, &files)?;

        let list = out.with_extension("concat.txt");
        let total_bytes = edit::write_concat_list(&files, &list)?;
        // The list is removed whether the copy succeeded or not.
        let concat = edit::concat_lossless_sized(
            bin,
            &list,
            out,
            total_bytes,
            layout.audio_count(),
        );
        let _ = std::fs::remove_file(&list);
        concat?;

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
