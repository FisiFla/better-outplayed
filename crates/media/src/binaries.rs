//! Locating and invoking the ffmpeg sidecar binaries.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// The directory ffmpeg/ffprobe are expected to live in for a packaged build.
pub fn sidecar_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("binaries")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegBinaries {
    /// Resolve binaries: explicit dir, then sidecar dir, then `PATH`.
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let searched = vec![
            explicit.clone(),
            Some(sidecar_dir()),
            which_dir("ffmpeg"),
        ];
        let searched: Vec<PathBuf> = searched.into_iter().flatten().collect();
        Self::discover_from(explicit, &searched)
    }

    /// Testable core: `candidates` is the list of directories that were searched.
    /// Panics are avoided so the error message can list every location tried.
    pub fn discover_from(explicit: Option<PathBuf>, candidates: &[PathBuf]) -> Result<Self> {
        // An explicit directory is authoritative: if the caller named it, use it.
        if let Some(dir) = explicit {
            return Ok(Self {
                ffmpeg: dir.join(exe("ffmpeg")),
                ffprobe: dir.join(exe("ffprobe")),
            });
        }
        if candidates.is_empty() {
            bail!(
                "ffmpeg not found. Looked in the sidecar directory ({}), PATH, and any \
                 configured location. Run `cargo xtask sidecars fetch` or install ffmpeg.",
                sidecar_dir().display()
            );
        }
        for dir in candidates {
            let ffmpeg = dir.join(exe("ffmpeg"));
            let ffprobe = dir.join(exe("ffprobe"));
            if ffmpeg.is_file() && ffprobe.is_file() {
                return Ok(Self { ffmpeg, ffprobe });
            }
        }
        bail!(
            "ffmpeg not found. Searched: {}. Run `cargo xtask sidecars fetch` or install ffmpeg.",
            candidates
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Directory containing `ffmpeg` on `PATH`, if any.
fn which_dir(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .find(|dir| dir.join(exe(binary)).is_file())
}

/// Run a child with stdin closed and a hard timeout, capturing stdout/stderr.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ffmpeg child")?;
    wait_with_deadline(&mut child, timeout)?;
    child.wait_with_output().context("collecting ffmpeg output")
}

/// Run a child with `input` written to its stdin — then closed — and a hard timeout.
///
/// [`run_with_timeout`] cannot stand in for this: it hands the child an immediately-closed
/// stdin, so a probe that has to *encode* something would have ffmpeg read zero frames and
/// fail — for an encoder that works perfectly well. This variant feeds bytes first. `input`
/// is taken by value because the writer thread has to own it.
pub fn run_with_stdin(mut cmd: Command, input: Vec<u8>, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ffmpeg child")?;
    let mut stdin = child.stdin.take().context("child stdin unavailable")?;

    // The write happens on its own thread, and deliberately not here: a pipe's buffer is
    // only 64KiB on this development host and as little as 4KiB for a Windows anonymous
    // pipe, so a blocking write from this thread could stall *before* the deadline loop
    // below ever runs — which is the very hang the timeout exists to bound. The thread
    // ends when the bytes are in or the child is gone (a dead reader fails `write_all`
    // with a broken pipe), so nothing needs to join it.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // Dropping `stdin` at the end of this closure is the child's EOF.
    });

    wait_with_deadline(&mut child, timeout)?;
    child.wait_with_output().context("collecting ffmpeg output")
}

/// Poll `child` until it exits, killing it and failing if `timeout` elapses first.
///
/// Polling rather than blocking is what makes the timeout real: a wedged child would
/// otherwise park the caller forever.
fn wait_with_deadline(child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().context("polling ffmpeg child")? {
            Some(_) => return Ok(()),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("ffmpeg timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn explicit_path_wins() {
        let found = FfmpegBinaries::discover_from(Some(PathBuf::from("/custom/dir")), &[])
            .expect("explicit path is used verbatim");
        assert_eq!(found.ffmpeg, PathBuf::from("/custom/dir/ffmpeg"));
    }

    #[test]
    fn falls_back_to_path_lookup_and_reports_all_searched_locations() {
        let err = FfmpegBinaries::discover_from(None, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sidecar"), "error must name the sidecar dir: {msg}");
        assert!(msg.contains("PATH"), "error must name PATH: {msg}");
    }
}
