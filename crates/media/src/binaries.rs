//! Locating and invoking the ffmpeg sidecar binaries.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// Name of the directory holding `ffmpeg`/`ffprobe`, in every layout localplay runs in.
///
/// It is the same word in all of them — a checkout, a `cargo run` target directory, and an
/// installed application — because the bundler's resource mapping is written against it
/// (`apps/desktop/src-tauri/tauri.conf.json`) and a test in the desktop crate pins the two
/// together. Change it here and the installer starts putting the sidecars somewhere the
/// lookup no longer reads.
pub const SIDECAR_DIR_NAME: &str = "binaries";

/// The directory ffmpeg/ffprobe are expected to live in for a packaged build: `binaries/`
/// next to the executable.
///
/// On Windows that is the whole story — the NSIS and MSI installers put resources in the
/// installation directory, i.e. beside the `.exe`. Inside a macOS `.app` it is not, because
/// the executable lives in `Contents/MacOS` and resources in `Contents/Resources`; that
/// second location is [`resource_sidecar_dirs_from`].
pub fn sidecar_dir() -> PathBuf {
    sidecar_dir_from(&exe_dir())
}

/// Testable core of [`sidecar_dir`].
fn sidecar_dir_from(exe_dir: &Path) -> PathBuf {
    exe_dir.join(SIDECAR_DIR_NAME)
}

/// The directories a *bundler* puts the sidecars in, for the executable at `exe_dir`.
///
/// Tauri's resource directory is not the same path on every platform, and that difference
/// decides whether an installed app can find its own ffmpeg:
///
/// - **Windows** (`nsis`/`msi`): resources are installed into the installation directory,
///   so `binaries/` lands next to the `.exe`. That is already [`sidecar_dir_from`], which
///   is why nothing is added here — Tauri documents its own `resource_dir` as "the
///   directory that contains the main executable" on Windows, and the installer's resource
///   mapping targets exactly that, so the first candidate on the search list covers it.
/// - **macOS** (`.app`): resources go to `<Foo>.app/Contents/Resources` while the
///   executable sits in `<Foo>.app/Contents/MacOS`. Different directory, so it has to be
///   named here — otherwise a packaged app would never look inside its own bundle, fall
///   through to `PATH`, and refuse to start on a machine that has no ffmpeg installed,
///   which is the entire point of the sidecar pipeline.
///
/// The `.app` shape is recognised from the directory structure rather than from
/// `cfg!(target_os)`: the rule is then testable on any host, and a bundle assembled on one
/// platform is found on the platform that runs it. Linux is deliberately absent — the
/// Phase 1 target is Windows only (spec §2), and an AppImage layout nothing here builds
/// would be a lookup path with no way to verify it.
pub fn resource_sidecar_dirs_from(exe_dir: &Path) -> Vec<PathBuf> {
    let in_macos_bundle = exe_dir.file_name() == Some(OsStr::new("MacOS"))
        && exe_dir.parent().and_then(Path::file_name) == Some(OsStr::new("Contents"));
    if !in_macos_bundle {
        return Vec::new();
    }
    match exe_dir.parent() {
        Some(contents) => vec![contents.join("Resources").join(SIDECAR_DIR_NAME)],
        None => Vec::new(),
    }
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
/// Priority is unaffected: this is only ever consulted *after* both installed locations
/// ([`sidecar_dir_from`] and [`resource_sidecar_dirs_from`]), so a shipped sidecar can
/// never be shadowed by a stale checkout. The walk also stops at the first `Cargo.toml` it
/// finds, so no ancestor directory outside a project root can supply binaries by accident.
///
/// It is deliberately *not* enough on its own for an installed app: a `Cargo.toml` above
/// the installation directory is not something an installer ever creates, and a shipped
/// app must not depend on one existing.
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
/// be asserted without a filesystem: the location next to the executable, then the resource
/// directory of an installed bundle, then the development checkout, then whatever is on
/// `PATH`.
///
/// The order is the whole contract. Everything an installer can produce outranks everything
/// a developer's machine can produce, so an application that was installed is never served
/// a stale copy from a checkout that happens to sit above it, and `PATH` — the one source
/// localplay cannot vouch for, since the LGPL-only, hardware-encoder-only build the spec
/// assumes (spec §3; README, "A note on the `ffmpeg` build") is not what an arbitrary
/// system ffmpeg is — comes last.
fn search_candidates(exe_dir: &Path, path_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = vec![sidecar_dir_from(exe_dir)];
    dirs.extend(resource_sidecar_dirs_from(exe_dir));
    dirs.extend(dev_sidecar_dir_from(exe_dir));
    dirs.extend(path_dir);
    dedupe(dirs)
}

/// Drop repeated directories, keeping the first occurrence.
///
/// The lists involved are at most four long, so a linear scan is the honest implementation.
/// The reason to bother at all: on Windows the *bundler's* resource directory and
/// "next to the executable" are the same path, which would otherwise print twice in the
/// "Searched:" list of a failure message.
fn dedupe(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// A `Command` for one of the sidecars, with no console window on Windows.
///
/// **Load-bearing on Windows, and it was missing.** A GUI process — which `localplay.exe` is —
/// allocates a console for every child it spawns unless the child is created with
/// `CREATE_NO_WINDOW`. So each ffmpeg, ffprobe or `reg` call put a terminal window on screen, and
/// the application makes a dozen of them while drawing a screen. Measured by the first person to
/// install it: *"terminal windows opening and closing"*, a machine that nearly fell over, and a
/// window that could not be resized or dragged — because the storm was holding the main thread.
/// The window config was never at fault; it has always said `resizable: true`.
///
/// Every subprocess in this workspace goes through here. The flag is a no-op off Windows.
pub fn sidecar_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console is allocated for the child. `CREATE_NO_WINDOW`.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegBinaries {
    /// Resolve binaries: an explicit directory, then next to the executable, then the
    /// resources of an installed bundle, then a checkout's `binaries/`, then `PATH`.
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
                "ffmpeg not found. Looked next to the executable ({}), in the resources of an \
                 installed app, in a checkout's `binaries/` directory above it (a \
                 development-only fallback), and on PATH. Run `cargo xtask sidecars fetch` \
                 or install ffmpeg.",
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
            "ffmpeg not found. Searched, in order: {}. That order is: next to the executable \
             (where the bundled resources of an installed app are placed on Windows), then \
             the resources of an installed macOS `.app` (`Contents/Resources`), then a \
             checkout's `binaries/` directory above the executable (a development-only \
             fallback), then PATH. Run `cargo xtask sidecars fetch`, or install ffmpeg.",
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
///
/// Public because one caller cannot use the two helpers above: the encoder's startup
/// throughput probe (`localplay_encoder::throughput`) feeds frames *in a loop for a bounded
/// window*, so it owns its own writing and needs exactly this from here — a deadline that
/// really ends, and a child that is killed and reaped rather than leaked when it expires.
pub fn wait_with_deadline(child: &mut Child, timeout: Duration) -> Result<()> {
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
            .expect("the configured directory is used");

        // The configured directory decides *where* the binaries are; the platform decides what
        // an executable is called. So this asserts the parent and the stem rather than one
        // literal path.
        //
        // It used to demand `/custom/dir/ffmpeg` exactly, and failed on Windows, where the
        // resolved path came back as `/custom/dir\ffmpeg.exe` — the `.exe` being a real and
        // legitimate difference, since a bare `ffmpeg` is not an executable there. (Measured on
        // the Windows test box; the assertion was about the host, not about which directory wins,
        // which is the property its name claims.)
        assert_eq!(found.ffmpeg.parent(), Some(std::path::Path::new("/custom/dir")));
        assert_eq!(found.ffmpeg.file_stem().and_then(|s| s.to_str()), Some("ffmpeg"));
        assert_eq!(
            found.ffmpeg.extension().and_then(|s| s.to_str()),
            if cfg!(windows) { Some("exe") } else { None },
            "a Windows executable is named ffmpeg.exe; elsewhere it is named ffmpeg"
        );
        assert_eq!(
            found.ffprobe.parent(),
            found.ffmpeg.parent(),
            "and ffprobe resolves beside it, not from PATH"
        );
        assert_eq!(found.ffprobe.file_stem().and_then(|s| s.to_str()), Some("ffprobe"));
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
        assert!(
            msg.contains("resources of an installed app"),
            "error must name the installed layout too, or a user of a broken installer has \
             nothing to look at: {msg}"
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

    /// Write a complete `ffmpeg`/`ffprobe` pair into `dir`, creating it if needed.
    fn write_sidecar_pair(dir: &Path) {
        std::fs::create_dir_all(dir).expect("creating a sidecar directory");
        std::fs::write(dir.join(exe("ffmpeg")), b"binary").expect("writing ffmpeg");
        std::fs::write(dir.join(exe("ffprobe")), b"binary").expect("writing ffprobe");
    }

    /// The layout `tauri build` produces inside a macOS `.app` for a `bundle.resources`
    /// mapping of `binaries/`: the executable in `<Foo>.app/Contents/MacOS`, the sidecars
    /// in `<Foo>.app/Contents/Resources/binaries`. Returns `(exe_dir, resource_dir)`.
    ///
    /// This is the tree that a packaged app actually runs in — see `docs/packaging.md` for
    /// the `ls` of a real bundle it was checked against.
    fn fake_installed_app(root: &Path) -> (PathBuf, PathBuf) {
        let contents = root.join("localplay.app").join("Contents");
        let exe_dir = contents.join("MacOS");
        std::fs::create_dir_all(&exe_dir).expect("creating Contents/MacOS");
        let resources = contents.join("Resources").join(SIDECAR_DIR_NAME);
        write_sidecar_pair(&resources);
        (exe_dir, resources)
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

    /// The load-bearing test for packaging.
    ///
    /// `Contents/Resources/binaries` is **not** "next to the executable" — it is where the
    /// bundler's `resources` mapping lands — so a lookup that only knew about
    /// `exe_dir/binaries` would fall through to `PATH` in every installed copy of the app,
    /// and refuse to start on a machine with no system ffmpeg. That is the exact defect this
    /// test exists to make impossible to ship again.
    #[test]
    fn installed_app_finds_the_sidecars_in_its_own_bundle_resources() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());

        let found = FfmpegBinaries::discover_from(
            None,
            &search_candidates(&exe_dir, Some(PathBuf::from("/usr/bin"))),
        )
        .expect("an installed app must find the sidecars it was bundled with");

        assert_eq!(found.ffmpeg, resources.join(exe("ffmpeg")));
        assert_eq!(found.ffprobe, resources.join(exe("ffprobe")));
        assert!(
            found.ffmpeg.starts_with(tmp.path().join("localplay.app")),
            "the resolved binary must be inside the application bundle, was {}",
            found.ffmpeg.display()
        );
    }

    /// The Windows-shaped installation: resources installed *beside* the executable, so
    /// `binaries/` sits in the installation directory and the plain `exe_dir` rule finds it.
    /// Nothing platform-specific is needed for this — which is why the test pins the shape
    /// rather than a code path: if the first candidate ever moved, this would notice.
    #[test]
    fn a_windows_style_install_finds_the_sidecars_next_to_the_executable() {
        let tmp = TempDir::new().expect("temp dir");
        let install = tmp.path().join("Program Files").join("localplay");
        std::fs::create_dir_all(&install).expect("creating the installation directory");
        write_sidecar_pair(&install.join(SIDECAR_DIR_NAME));

        let found = FfmpegBinaries::discover_from(None, &search_candidates(&install, None))
            .expect("the installer's own layout must be found");

        assert_eq!(found.ffmpeg, install.join(SIDECAR_DIR_NAME).join(exe("ffmpeg")));
    }

    #[test]
    fn a_bundle_searches_its_resources_before_the_checkout_and_before_path() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        // The app sits inside a developer's checkout, so the development fallback is live
        // too: the bundle's resources must still come first.
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        let checkout = tmp.path().join("binaries");
        write_sidecar_pair(&checkout);
        assert_eq!(
            search_candidates(&exe_dir, Some(PathBuf::from("/usr/bin"))),
            vec![exe_dir.join(SIDECAR_DIR_NAME), resources, checkout, PathBuf::from("/usr/bin")],
            "order is: next to the executable, then the bundle's resources, then the \
             checkout, then PATH"
        );
    }

    /// Precedence with every source populated at once, resolved rather than merely ordered.
    #[test]
    fn an_installed_sidecar_outranks_the_checkout_and_path() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        let stale_checkout = tmp.path().join("binaries");
        write_sidecar_pair(&stale_checkout);
        let on_path = tmp.path().join("on-path");
        write_sidecar_pair(&on_path);

        let found =
            FfmpegBinaries::discover_from(None, &search_candidates(&exe_dir, Some(on_path.clone())))
                .expect("one of the four sources has the pair");

        assert_eq!(
            found.ffmpeg,
            resources.join(exe("ffmpeg")),
            "the shipped sidecar must win over both the checkout and PATH"
        );
        assert_ne!(found.ffmpeg, stale_checkout.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, on_path.join(exe("ffmpeg")));
    }

    #[test]
    fn the_checkout_fallback_outranks_path_when_nothing_is_installed() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, checkout) = fake_checkout(tmp.path());
        write_sidecar_pair(&checkout);
        let on_path = tmp.path().join("on-path");
        write_sidecar_pair(&on_path);

        let found =
            FfmpegBinaries::discover_from(None, &search_candidates(&exe_dir, Some(on_path.clone())))
                .expect("the checkout pair is found");

        assert_eq!(found.ffmpeg, checkout.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, on_path.join(exe("ffmpeg")));
    }

    #[test]
    fn a_pair_next_to_the_executable_outranks_the_bundle_resources() {
        // Reachable if something puts a pair in `Contents/MacOS/binaries` (a hand-assembled
        // folder, or Tauri's `externalBin` mechanism, which copies into `Contents/MacOS`).
        // Stated as a rule so the order is a decision rather than an accident of how the
        // candidate list happens to be built.
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        let beside_the_exe = exe_dir.join(SIDECAR_DIR_NAME);
        write_sidecar_pair(&beside_the_exe);

        let found = FfmpegBinaries::discover_from(None, &search_candidates(&exe_dir, None))
            .expect("the pair next to the executable is found");

        assert_eq!(found.ffmpeg, beside_the_exe.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, resources.join(exe("ffmpeg")));
    }

    #[test]
    fn a_directory_that_is_not_an_app_bundle_has_no_resource_location() {
        // A false positive here would put a directory that cannot exist on the search list
        // and print it in every "ffmpeg not found" message.
        let tmp = TempDir::new().expect("temp dir");
        let install = tmp.path().join("Program Files/localplay");
        std::fs::create_dir_all(&install).expect("creating the installation directory");
        assert!(resource_sidecar_dirs_from(&install).is_empty());

        // `MacOS` is not enough on its own: the parent has to be a `Contents` directory.
        let not_a_bundle = tmp.path().join("somewhere/MacOS");
        std::fs::create_dir_all(&not_a_bundle).expect("creating somewhere/MacOS");
        assert!(resource_sidecar_dirs_from(&not_a_bundle).is_empty());
    }

    /// The same lookup, but pointed at a bundle that was actually built rather than a tree
    /// this test wrote: the difference between "the rule is right" and "the artifact is
    /// shaped the way the rule expects".
    ///
    /// Opt-in, because it needs a build artifact that CI does not produce and that no unit
    /// test should depend on. Point it at a bundle and it resolves the executable's own
    /// `Contents/MacOS` directory through the production code path:
    ///
    /// ```text
    /// LOCALPLAY_APP_BUNDLE=apps/desktop/src-tauri/target/release/bundle/macos/localplay.app \
    ///   cargo test -p localplay-media --lib a_real_app_bundle_beats_path
    /// ```
    ///
    /// `which_dir` is real (not a made-up directory), so when it finds a system ffmpeg this
    /// also demonstrates that the copy inside the bundle wins over `PATH`.
    #[test]
    fn a_real_app_bundle_beats_path() {
        let Ok(app) = std::env::var("LOCALPLAY_APP_BUNDLE") else {
            return;
        };
        let app = PathBuf::from(app);
        let exe_dir = app.join("Contents").join("MacOS");
        assert!(
            exe_dir.is_dir(),
            "LOCALPLAY_APP_BUNDLE must point at a `.app` directory: {} has no Contents/MacOS",
            app.display()
        );

        let candidates = search_candidates(&exe_dir, which_dir("ffmpeg"));
        let found = FfmpegBinaries::discover_from(None, &candidates)
            .unwrap_or_else(|e| panic!("nothing found in the built bundle {}\n{e}", app.display()));

        assert_eq!(
            found.ffmpeg,
            app.join("Contents").join("Resources").join(SIDECAR_DIR_NAME).join(exe("ffmpeg"))
        );
        assert!(found.ffprobe.is_file(), "{:?} exists too", found.ffprobe);
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
