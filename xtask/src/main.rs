//! Build helpers: sidecar acquisition and encoder probing.

mod sidecars;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
        _ => bail!(usage()),
    }
}

fn usage() -> &'static str {
    "usage:\n  \
     xtask sidecars record [--target <triple>]   # download, hash, rewrite sidecars.toml\n  \
     xtask sidecars fetch  [--target <triple>]   # verify the recorded hash, then extract\n  \
     xtask probe                                 # which hardware encoders this machine can run"
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
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("running `ffmpeg -encoders`")?;
    let text = String::from_utf8_lossy(&out.stdout);

    println!("{}", bin.ffmpeg.display());
    let mut usable = 0;
    for name in ["h264_nvenc", "hevc_nvenc", "h264_qsv", "hevc_qsv", "h264_amf", "hevc_amf"] {
        // Two questions, and only the second one is the answer. `-encoders` says the build
        // carries the encoder; the smoke test says *this machine* can run it, which is
        // what a `vendor` value in the config depends on. A vendor can advertise an
        // encoder it cannot initialise — measured on a box with no AMD hardware, ffmpeg
        // listed h264_amf and died with `DLL amfrt64.dll failed to open` when asked to use
        // it — so a row that says only "listed" is not enough to trust.
        let state = if !text.lines().any(|l| l.contains(name)) {
            "not advertised".to_string()
        } else {
            match localplay_media::smoke_test_encoder(&bin, name) {
                Ok(()) => {
                    usable += 1;
                    "advertised, WORKS".to_string()
                }
                Err(reason) => format!("advertised, FAILS: {reason}"),
            }
        };
        println!("{name:<12} {state}");
    }
    println!(
        "\nWORKS means ffmpeg listed it AND encoded one 320x240 frame with it. A vendor value \
         only works when it lands on WORKS. This is a synthetic frame: it does not capture \
         the screen and does not inject input."
    );
    if usable == 0 {
        println!(
            "No hardware encoder on this machine can encode a frame; the capture app will \
             refuse to start rather than fall back to CPU encoding."
        );
    }
    Ok(())
}
