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
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("running `ffmpeg -encoders`; is ffmpeg on PATH?")?;
    let text = String::from_utf8_lossy(&out.stdout);
    for name in ["h264_nvenc", "hevc_nvenc", "h264_qsv", "hevc_qsv", "h264_amf", "hevc_amf"] {
        let present = text.lines().any(|l| l.contains(name));
        println!("{name:<12} {}", if present { "listed" } else { "absent" });
    }
    println!("\n\"listed\" means ffmpeg advertises it; a 1-frame smoke test is still required.");
    Ok(())
}
