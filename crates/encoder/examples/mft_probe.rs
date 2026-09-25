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
        if encoder.accepts.is_empty() {
            println!("      takes no candidate input format");
        } else {
            for subtype in &encoder.accepts {
                println!("      takes: {subtype}");
            }
        }
        if encoder.input_subtypes.is_empty() {
            println!("      (it enumerates no types, which is what an async MFT does)");
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
    // The encoder core itself, at the resolution that matters. Everything above is a query; this
    // is the thing: configure a 4K encoder, hand it a GPU texture, and see whether it takes it
    // without a single pixel crossing the CPU.
    if usable > 0 {
        println!("\n--- the encoder core, at 3840x2160 ---");
        let size = (3840u32, 2160u32);
        // The device is the *probe's* own here. In the real pipeline it is capture's, and that is
        // the whole reason `MftEncoder::open` takes one rather than making its own.
        let device = localplay_encoder::mft::create_capture_kind_device()?;
        match localplay_encoder::mft::MftEncoder::open(&device, size) {
            Ok(mut encoder) => {
                println!(
                    "opened {} for {size:?}, taking {} input",
                    encoder.encoder_name, encoder.input_format
                );
                if !encoder.input_format.starts_with("RGB32") && !encoder.input_format.starts_with("ARGB32") {
                    println!(
                        "  NOTE: that is not the format capture delivers, so this chain needs a \
                         GPU-side conversion to reach it"
                    );
                }
                let texture = fake_frame(encoder.device(), size)?;
                match encoder.push_texture(&texture, 0) {
                    Ok(()) => println!("the encoder accepted a GPU texture with no CPU copy"),
                    Err(err) => println!("push_texture failed: {err:#}"),
                }
            }
            Err(err) => println!("could not open one for {size:?}: {err:#}"),
        }
    }

    if usable == 0 {
        // Not an error: it is a finding, and the one the plan most needs. Exit non-zero so a
        // script notices, but print the reason rather than a stack trace.
        anyhow::bail!("no hardware encoder on this machine accepts the D3D11 handshake");
    }
    Ok(())
}

/// A BGRA texture of `size` on `device`, standing in for a captured frame.
///
/// Zero-filled: what is being tested is whether the encoder will take a *surface* of this format at
/// this size, and the pixels' value has no bearing on that. Created with the bind flags Windows
/// Graphics Capture's own frames carry, because a texture the encoder refuses on those grounds
/// would look like a format refusal and is not one.
#[cfg(windows)]
fn fake_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    size: (u32, u32),
) -> anyhow::Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    use anyhow::Context;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `texture` is the out-parameter and the description outlives the call.
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture)) }
        .context("ID3D11Device::CreateTexture2D")?;
    texture.context("CreateTexture2D returned no texture")
}

#[cfg(not(windows))]
fn main() {
    println!(
        "The hardware encoder path is Windows-only: it is Media Foundation's encoder MFTs. \
         Nothing to probe on this host."
    );
}
