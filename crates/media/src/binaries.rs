//! Locating and invoking the ffmpeg sidecar binaries.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// The directory ffmpeg/ffprobe are expected to live in for a packaged build.
pub fn sidecar_dir() -> PathBuf {
    sidecar_dir_from(&exe_dir())
}

/// Testable core of [`sidecar_dir`].
fn sidecar_dir_from(exe_dir: &Path) -> PathBuf {
    exe_dir.join("binaries")
}

/// Directory holding the running executable, or `.` when it cannot be determined.
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// An extra, *development-only* place to look for the sidecars.
///
/// `cargo xtask sidecars fetch` writes `<repo>/binaries/`, but a `cargo run` executable
/// lives in `<repo>/target/<profile>/`, so the packaged-build lookup above misses it by two
/// directories. The alternative — having xtask guess a cargo profile directory and copy
/// ~250 MB of binaries into it — would duplicate the sidecars per profile and would break
/// the moment the profile or target directory changed. So instead this walks *up* from the
/// executable until it meets a directory holding a `Cargo.toml`, i.e. the root of a
/// checkout rather than an installation, and uses that directory's `binaries/` if it is
/// there.
///
/// Priority is unaffected: this is only ever consulted *after* the directory next to the
/// executable, so an installed sidecar can never be shadowed by a stale checkout. The walk
/// also stops at the first `Cargo.toml` it finds, so no ancestor directory outside a
/// project root can supply binaries by accident.
pub fn dev_sidecar_dir() -> Option<PathBuf> {
    dev_sidecar_dir_from(&exe_dir())
}

/// Testable core of [`dev_sidecar_dir`]: `exe_dir` is the directory holding the executable.
pub fn dev_sidecar_dir_from(exe_dir: &Path) -> Option<PathBuf> {
    /// Enough for `target/<profile>/` and `target/<triple>/<profile>/`, and then some.
    const MAX_LEVELS: usize = 6;
    let mut current = Some(exe_dir);
    for _ in 0..MAX_LEVELS {
        let dir = current?;
        if dir.join("Cargo.toml").is_file() {
            let sidecars = dir.join("binaries");
            return sidecars.is_dir().then_some(sidecars);
        }
        current = dir.parent();
    }
    None
}

/// The ordered list of directories [`discover`] searches. Pure, so the ordering rules can
/// be asserted without a filesystem: production location first, then the development
/// checkout, then whatever is on `PATH`.
fn search_candidates(exe_dir: &Path, path_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = vec![sidecar_dir_from(exe_dir)];
    dirs.extend(dev_sidecar_dir_from(exe_dir));
    dirs.extend(path_dir);
    dirs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegBinaries {
    /// Resolve binaries: explicit dir, then next to the executable, then the checkout's
    /// `binaries/`, then `PATH`.
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let candidates = search_candidates(&exe_dir(), which_dir("ffmpeg"));
        Self::discover_from(explicit, &candidates)
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
                "ffmpeg not found. Looked next to the executable ({}), in a checkout's \
                 `binaries/` directory above it (a development-only fallback), and on PATH. \
                 Run `cargo xtask sidecars fetch` or install ffmpeg.",
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
    use tempfile::TempDir;

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
        assert!(
            msg.contains("binaries"),
            "error must name the checkout's binaries/ fallback: {msg}"
        );
    }

    #[test]
    fn error_lists_every_location_it_searched() {
        let err = FfmpegBinaries::discover_from(
            None,
            &[PathBuf::from("/nowhere/next-to-exe"), PathBuf::from("/nowhere/checkout")],
        )
        .expect_err("neither directory has the binaries");
        let msg = err.to_string();
        assert!(msg.contains("/nowhere/next-to-exe"), "{msg}");
        assert!(msg.contains("/nowhere/checkout"), "{msg}");
        assert!(msg.contains("cargo xtask sidecars fetch"), "{msg}");
    }

    /// A checkout laid out like the real one: `Cargo.toml` and `binaries/` at the root, the
    /// running executable two levels down in `target/debug/`.
    fn fake_checkout(root: &Path) -> (PathBuf, PathBuf) {
        let exe_dir = root.join("target/debug");
        std::fs::create_dir_all(&exe_dir).expect("creating target/debug");
        std::fs::create_dir_all(root.join("binaries")).expect("creating binaries/");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        (exe_dir, root.join("binaries"))
    }

    #[test]
    fn dev_fallback_finds_the_checkout_binaries_above_target_debug() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, expected) = fake_checkout(tmp.path());
        assert_eq!(dev_sidecar_dir_from(&exe_dir), Some(expected));
    }

    #[test]
    fn dev_fallback_finds_the_checkout_from_a_test_binary_in_deps() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, expected) = fake_checkout(tmp.path());
        let deps = exe_dir.join("deps");
        std::fs::create_dir_all(&deps).expect("creating deps");
        assert_eq!(dev_sidecar_dir_from(&deps), Some(expected));
    }

    #[test]
    fn dev_fallback_stops_at_the_first_cargo_manifest() {
        // A nested package without its own binaries/ must not reach past itself into an
        // outer checkout's copy — that is how a stale one would shadow the right one.
        let tmp = TempDir::new().expect("temp dir");
        let (_outer_exe, _outer_binaries) = fake_checkout(tmp.path());
        let inner = tmp.path().join("crates/inner");
        let inner_exe = inner.join("target/debug");
        std::fs::create_dir_all(&inner_exe).expect("creating inner target/debug");
        std::fs::write(inner.join("Cargo.toml"), "[package]\n").expect("writing inner Cargo.toml");
        assert_eq!(
            dev_sidecar_dir_from(&inner_exe),
            None,
            "the walk must stop at the inner manifest, not climb into the outer checkout"
        );
    }

    #[test]
    fn dev_fallback_gives_up_without_a_checkout() {
        let tmp = TempDir::new().expect("temp dir");
        let exe_dir = tmp.path().join("Program Files/localplay");
        std::fs::create_dir_all(&exe_dir).expect("creating install dir");
        // No Cargo.toml on the way up: an installed layout must not acquire a fallback.
        assert_eq!(dev_sidecar_dir_from(&exe_dir), None);
    }

    #[test]
    fn production_sidecar_dir_is_searched_before_the_dev_checkout_and_before_path() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, dev) = fake_checkout(tmp.path());
        let candidates = search_candidates(&exe_dir, Some(PathBuf::from("/usr/bin")));
        assert_eq!(
            candidates,
            vec![exe_dir.join("binaries"), dev, PathBuf::from("/usr/bin")],
            "order is: next to the executable, then the checkout, then PATH"
        );
    }

    #[test]
    fn installed_sidecar_outranks_the_checkout_copy() {
        let tmp = TempDir::new().expect("temp dir");
        let installed = tmp.path().join("installed/binaries");
        let checkout = tmp.path().join("checkout/binaries");
        for dir in [&installed, &checkout] {
            std::fs::create_dir_all(dir).expect("creating a binaries dir");
            std::fs::write(dir.join(exe("ffmpeg")), b"binary").expect("writing ffmpeg");
            std::fs::write(dir.join(exe("ffprobe")), b"binary").expect("writing ffprobe");
        }
        let found = FfmpegBinaries::discover_from(None, &[installed.clone(), checkout.clone()])
            .expect("the first complete directory wins");
        assert_eq!(
            found.ffmpeg,
            installed.join(exe("ffmpeg")),
            "an installed sidecar must never be shadowed by a checkout copy"
        );
        assert_ne!(found.ffmpeg, checkout.join(exe("ffmpeg")));
    }

    #[test]
    fn a_directory_holding_only_one_of_the_two_binaries_is_not_used() {
        let tmp = TempDir::new().expect("temp dir");
        let half = tmp.path().join("half/binaries");
        std::fs::create_dir_all(&half).expect("creating a binaries dir");
        std::fs::write(half.join(exe("ffmpeg")), b"binary").expect("writing ffmpeg");
        let err = FfmpegBinaries::discover_from(None, std::slice::from_ref(&half))
            .expect_err("half a pair is not a sidecar directory");
        assert!(err.to_string().contains("half"), "{err}");
    }
}
