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
        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }
}
