//! Lossless (stream-copy) media edits. These never re-encode.

use crate::binaries::{run_with_timeout, FfmpegBinaries};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const EDIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Remux without re-encoding, moving the index to the front for fast seeking.
pub fn remux_lossless(bin: &FfmpegBinaries, src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "remux", dst)
}

/// Trim `[start_ms, end_ms)` with `-c copy`.
///
/// Cuts snap to the nearest preceding keyframe — this is inherent to stream copy,
/// not a bug. See spec §6.3.
pub fn trim_lossless(
    bin: &FfmpegBinaries,
    src: &Path,
    dst: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<()> {
    if end_ms <= start_ms {
        bail!("trim range is empty: start={start_ms}ms end={end_ms}ms");
    }
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", start_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .arg("-t")
        .arg(format!("{:.3}", (end_ms - start_ms) as f64 / 1000.0))
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "trim", dst)
}

/// Single-frame JPEG at `at_ms`, for Phase 2 timelines.
pub fn thumbnail(bin: &FfmpegBinaries, src: &Path, at_ms: u64, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", at_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .args(["-frames:v", "1", "-q:v", "4"])
        .arg(dst);
    expect_success(cmd, "thumbnail", dst)
}

/// Concatenate segments into one file with `-c copy`.
///
/// Every segment must already share identical codec parameters — they do, because
/// they come from one encoder invocation (spec §6.1).
pub fn concat_lossless(bin: &FfmpegBinaries, list_file: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_file)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "concat", dst)
}

fn expect_success(mut cmd: Command, what: &str, dst: &Path) -> Result<()> {
    let out = run_with_timeout(cmd, EDIT_TIMEOUT)?;
    if !out.status.success() {
        bail!(
            "{what} failed writing {}: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!("{what} reported success but {} does not exist", dst.display());
    }
    dst.metadata()
        .map(|_| ())
        .with_context(|| format!("statting {}", dst.display()))
}
