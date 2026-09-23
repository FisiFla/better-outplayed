//! Build helpers: sidecar acquisition and encoder probing.

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
    match std::env::args().nth(1).as_deref() {
        Some("sidecars") => match std::env::args().nth(2).as_deref() {
            Some("fetch") => sidecars_fetch(),
            Some("record") => sidecars_record(),
            other => bail!("usage: xtask sidecars <fetch|record>, got {other:?}"),
        },
        Some("probe") => probe_encoders(),
        _ => bail!("usage: xtask <sidecars|probe>"),
    }
}

fn binaries_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../binaries")
}

fn sidecars_fetch() -> Result<()> {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecars.toml"),
    )
    .context("reading sidecars.toml")?;

    if manifest.contains("RECORD_ME") {
        bail!(
            "sidecars.toml has an unrecorded sha256. Run `cargo xtask sidecars record` on a \
             trusted network, review the diff, and commit the recorded hash. Fetching will \
             then extract the archives into {}.",
            binaries_dir().display()
        );
    }
    // Download to a temp file, verify sha256, then extract into binaries/.
    // Deliberately not implemented with a placeholder: see the note below.
    bail!(
        "not implemented in this task; when implemented it will verify the archive against \
         the recorded sha256 and extract into {} — see the note in the plan",
        binaries_dir().display()
    )
}

fn sidecars_record() -> Result<()> {
    bail!("not implemented in this task; see the note in the plan")
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
