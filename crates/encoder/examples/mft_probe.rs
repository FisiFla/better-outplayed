//! Ask this machine what hardware video encoders it has, and whether they take a GPU texture.
//!
//! The same shape as `xtask probe`, and for the same reason: it is the most that can be learned
//! about the hardware encode path without opening a window or starting a capture session. It
//! starts Media Foundation, enumerates the hardware H.264 encoder MFTs, and for each one runs the
//! handshake the Tier 2 zero-copy encoder will have to run — passing it a device manager built
//! over a D3D11 device of the kind Windows Graphics Capture requires.
//!
//! Run it on the machine in question:
//!
//! ```text
//! cargo run --release -p localplay-encoder --example mft_probe
//! ```
//!
//! What it answers: whether the design in `docs/plans/2026-09-25-localplay-tier-2-zero-copy.md`
//! has anything to stand on. A machine with no hardware encoder MFT, or one whose MFT refuses the
//! D3D11 handshake, cannot take this path at all — and that is much cheaper to learn here than
//! after the encoder is written.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let encoders = localplay_encoder::mft::probe_hardware_encoders()?;
    println!("hardware H.264 encoder MFTs: {}", encoders.len());
    for encoder in &encoders {
        println!(
            "  {:<46} d3d11={:<5} async={:<5}\n      url: {}",
            encoder.name,
            encoder.accepts_d3d11,
            encoder.asynchronous,
            encoder.hardware_url.as_deref().unwrap_or("-"),
        );
        if encoder.input_subtypes.is_empty() {
            println!("      input types: (could not be asked)");
        } else {
            for subtype in &encoder.input_subtypes {
                println!("      accepts: {subtype}");
            }
        }
        if let Some(why) = &encoder.refusal {
            println!("      refused: {why}");
        }
    }
    let usable = encoders.iter().filter(|e| e.accepts_d3d11).count();
    println!("\nof those, {usable} accepted MFT_MESSAGE_SET_D3D_MANAGER over a BGRA-capable D3D11 device");

    // Whether frames would need converting before an encoder will take them. WGC delivers BGRA8; an
    // encoder that lists NV12 but not RGB32 means a Video Processor MFT belongs in the chain, on the
    // GPU, rather than a CPU conversion loop — which is the cost this plan exists to remove.
    let takes_bgra = encoders.iter().any(|e| {
        e.input_subtypes
            .iter()
            .any(|s| s.starts_with("RGB32") || s.starts_with("ARGB32"))
    });
    println!("any encoder takes BGRA/ARGB directly: {takes_bgra}");
    match localplay_encoder::mft::probe_video_processors() {
        Ok(processors) if processors.is_empty() => {
            println!("video processor MFTs (for a GPU-side colour conversion): none")
        }
        Ok(processors) => println!(
            "video processor MFTs (for a GPU-side colour conversion): {}",
            processors.join(", ")
        ),
        Err(err) => println!("video processor MFTs: could not be asked: {err}"),
    }
    if usable == 0 {
        // Not an error: it is a finding, and the one the plan most needs. Exit non-zero so a
        // script notices, but print the reason rather than a stack trace.
        anyhow::bail!("no hardware encoder on this machine accepts the D3D11 handshake");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!(
        "The hardware encoder path is Windows-only: it is Media Foundation's encoder MFTs. \
         Nothing to probe on this host."
    );
}
