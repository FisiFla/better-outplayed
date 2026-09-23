//! Build helpers: sidecar acquisition, encoder probing, and the verification harness.

mod probe;
mod sidecars;
mod verify;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sidecars") => sidecars_command(&args[1..]),
        Some("probe") => probe_encoders(),
        Some("verify") => verify::verify_command(&args[1..]),
        _ => bail!(usage()),
    }
}

fn usage() -> &'static str {
    "usage:\n  \
     xtask sidecars record [--target <triple>]   # download, hash, rewrite sidecars.toml\n  \
     xtask sidecars fetch  [--target <triple>]   # verify the recorded hash, then extract\n  \
     xtask probe                                 # which hardware encoders this machine can run\n  \
     xtask verify [--seconds N] [--report PATH]  # run the Phase 1 acceptance checks, write a report"
}

/// The repository root. `xtask` is a workspace member, so its manifest sits one level down.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where the extracted sidecars go: next to the executable in a packaged build, and next to
/// the repository root in a checkout, where `localplay-media`'s development fallback finds it.
fn binaries_dir() -> PathBuf {
    repo_root().join("binaries")
}

/// Scratch space for downloads. Under `target/`, which is gitignored, so a 120 MB archive
/// cannot be committed by accident.
fn work_dir() -> PathBuf {
    repo_root().join("target/sidecars")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecars.toml")
}

/// `--target <triple>` / `--target=<triple>`, or nothing.
fn parse_target(args: &[String]) -> Result<Option<String>> {
    let mut target = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(value) = arg.strip_prefix("--target=") {
            target = Some(value.to_string());
        } else if arg == "--target" {
            let value = it
                .next()
                .context("--target needs a value, e.g. --target x86_64-pc-windows-msvc")?;
            target = Some(value.clone());
        } else {
            bail!("unexpected argument {arg:?}\n{}", usage());
        }
    }
    Ok(target)
}

fn sidecars_command(args: &[String]) -> Result<()> {
    let (sub, rest) = args
        .split_first()
        .context("usage: xtask sidecars <fetch|record> [--target <triple>]")?;
    let target = parse_target(rest)?;
    match sub.as_str() {
        // `--target` on `record` exists for the same reason it does on `fetch`: a machine
        // that is not the packaging machine records, reviews and fetches the Windows entry.
        "record" => {
            sidecars::record(&manifest_path(), &binaries_dir(), &work_dir(), target.as_deref())
        }
        "fetch" => sidecars::fetch(
            &manifest_path(),
            &binaries_dir(),
            target.as_deref(),
            sidecars::host_target().as_deref(),
            &work_dir(),
        ),
        other => bail!("usage: xtask sidecars <fetch|record>, got {other:?}"),
    }
}

fn probe_encoders() -> Result<()> {
    let bin = localplay_media::FfmpegBinaries::discover(None)
        .context("locating ffmpeg; is it on PATH, or in the sidecar directory?")?;
    print!("{}", probe::probe(&bin)?.render());
    Ok(())
}
