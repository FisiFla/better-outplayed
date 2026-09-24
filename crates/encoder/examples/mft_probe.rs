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
        if let Some(why) = &encoder.refusal {
            println!("      refused: {why}");
        }
    }
    let usable = encoders.iter().filter(|e| e.accepts_d3d11).count();
    println!("\nof those, {usable} accepted MFT_MESSAGE_SET_D3D_MANAGER over a BGRA-capable D3D11 device");
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
