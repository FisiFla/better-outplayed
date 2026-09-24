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
        log_drift(out, &info);

        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }

    /// Concatenate a **range of in-memory fragments** into one lossless clip file.
    ///
    /// The counterpart of [`ClipSplicer::splice`] for a RAM ring, and deliberately the same
    /// shape: the same `-c copy`, the same refusal to re-encode, the same A/V drift report, the
    /// same `ClipMetadata`. The only difference is where the footage comes from — bytes that
    /// have never been written to disk, instead of a list of files that already were.
    ///
    /// Everything the window carries is checked before ffmpeg is spawned, because each is a
    /// silent-damage case rather than an error the muxer would raise:
    ///
    /// * **an empty window** has nothing to cut;
    /// * **a first fragment that is not a keyframe** would make the clip open mid-GOP, and the
    ///   first frames would decode as garbage (or not at all). `frag_keyframe` means this cannot
    ///   happen from this ring — which is exactly why it is asserted rather than assumed;
    /// * **an unclosed window** would put a fragment whose length nothing has proved at the tail
    ///   of a file whose duration then lies. [`crate::MemoryRingBuffer::window`] already selects
    ///   only closed fragments; this checks the invariant at the point of use.
    pub fn splice_from_memory(
        bin: &FfmpegBinaries,
        window: &crate::ram_buffer::MemoryWindow<'_>,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        let Some(first) = window.segments.first() else {
            bail!("cannot splice an empty in-memory window");
        };
        if !first.keyframe {
            bail!(
                "the first fragment of this window starts at {}ms and is not a keyframe, so a \
                 clip cut here would open mid-GOP",
                first.start_ms
            );
        }
        if window.segments.iter().any(|s| !s.is_closed()) {
            bail!(
                "this window contains a fragment whose end nothing has proved yet, so the \
                 clip's duration would be a claim rather than a measurement"
            );
        }

        let audio_tracks = window.audio_tracks();
        localplay_media::edit::remux_stream_lossless(bin, window.assemble(), out, audio_tracks)?;

        let info = localplay_media::MediaInfo::probe(bin, out)?;
        log_drift(out, &info);

        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }
}

/// Log the A/V offset of a clip that was just written.
///
/// One definition, used by both splice paths, because the two must report the same number the
/// same way: this is spec §11 criterion 8, and a reader comparing a RAM clip's log line against
/// a file clip's has to be comparing the same measurement.
///
/// A/V offset **within the produced clip** — it compares the audio and video stream timelines of
/// the muxed file. It is NOT the QPC-clock divergence between the two live capture sources,
/// which would require instrumenting the capture path itself.
fn log_drift(out: &Path, info: &localplay_media::MediaInfo) {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
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
}

/// Render an optional millisecond count for the log line, saying "unavailable"
/// rather than pretending a missing value is 0.
fn opt_ms(ms: Option<u64>) -> String {
    ms.map(|m| m.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}
