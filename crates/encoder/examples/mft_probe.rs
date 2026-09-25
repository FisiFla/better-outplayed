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

    // `cargo run --example mft_probe -- hevc` asks the same questions about HEVC. A codec is not a
    // flag on this probe's findings, it *is* the question: which MFTs will agree to produce it, and
    // whether the ARGB32 handshake survives the different profile and level it needs.
    let codec = if std::env::args().any(|arg| arg.eq_ignore_ascii_case("hevc")) {
        localplay_encoder::VideoCodec::Hevc
    } else {
        localplay_encoder::VideoCodec::H264
    };
    // Unfiltered, so that "no encoder for this codec" can be told apart from "the filter is wrong".
    match localplay_encoder::mft::list_hardware_video_encoders() {
        Ok(names) => {
            println!("--- every hardware video encoder MFT here, unfiltered: {} ---", names.len());
            for name in names {
                println!("  {name}");
            }
        }
        Err(err) => println!("could not list encoder MFTs: {err:#}"),
    }
    println!("--- asking about {codec:?} ---");
    let encoders = localplay_encoder::mft::probe_hardware_encoders(codec)?;
    println!("hardware {codec:?} encoder MFTs: {}", encoders.len());
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

        // **The format is the question for HEVC, so ask it directly.** The H.264 encoder takes the
        // BGRA texture below and converts inside itself; the HEVC one refuses that format outright
        // (`0xC00D6D76`, "the input type is not supported for D3D device"). Both are asked here, of
        // the same device, so the answer is about the format rather than about a guess — and NV12 is
        // what the capture handover will have to produce for the HEVC path to exist at all.
        // **And if the encoder still will not have it, find out what it objects to.** One attribute
        // at a time, because three guesses have already failed and a fourth would be luck.
        if codec == localplay_encoder::VideoCodec::Hevc {
            println!("\n--- which attribute of the output type it objects to ---");
            for (what, verdict) in localplay_encoder::mft::probe_output_types(codec, size)? {
                println!("  {what}\n     -> {verdict}");
            }
        }

        println!("\n--- what an NV12 texture does ---");
        let nv12 = fake_frame(
            &device,
            size,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
        );
        match nv12.and_then(|texture| localplay_encoder::mft::MftEncoder::open_for_texture(&texture, codec)) {
            Ok(encoder) => println!(
                "an NV12 texture opens: {} taking {}",
                encoder.encoder_name, encoder.input_format
            ),
            Err(err) => println!("an NV12 texture does not open: {err:#}"),
        }

        println!("\n--- which GPU ---");
        for (index, name) in localplay_encoder::mft::describe_adapters().into_iter().enumerate() {
            let note = if index == 0 {
                "   <- what a null-adapter device gets (the default)"
            } else {
                ""
            };
            println!("  [{index}] {name}{note}");
        }
        println!(
            "  capture-kind device: {}",
            localplay_encoder::mft::adapter_description(&device)
        );
        match localplay_encoder::mft::MftEncoder::open(
            &device,
            size,
            codec,
            Some(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM),
        ) {
            Ok(mut encoder) => {
                println!(
                    "opened {} for {size:?}, taking {} input, on adapter {}",
                    encoder.encoder_name,
                    encoder.input_format,
                    localplay_encoder::mft::adapter_description(encoder.device())
                );
                if !encoder.input_format.starts_with("RGB32") && !encoder.input_format.starts_with("ARGB32") {
                    println!(
                        "  NOTE: that is not the format capture delivers, so this chain needs a \
                         GPU-side conversion to reach it"
                    );
                }
                let texture = fake_frame(
                      encoder.device(),
                      size,
                      windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                  )?;
                encoder.start()?;
                // Feed it a second of frames and collect what comes out. What is being tested is
                // that H.264 bytes exist at all — that a captured texture reaches the encoder with
                // no CPU copy *and* that the encoder is willing to make a stream of it.
                let mut total = 0usize;
                let mut frames = 0usize;
                let mut stream: Vec<u8> = Vec::new();
                // Streamed into ffmpeg *as they are produced*, not collected and handed over at the
                // end. That distinction is the whole of this test: an H.264 elementary stream has no
                // timestamps, so the only ones ffmpeg can use are the times the bytes arrive — and a
                // batch write makes every frame arrive at once. Measured the wrong way first: 30
                // frames paced over a second produced a 196ms fragment, because the probe had
                // buffered them. The pipeline streams, so the probe has to as well.
                let mut sink = FfmpegSink::spawn()?;
                // Paced at 30fps, deliberately, for the same reason.
                let pace = std::time::Duration::from_millis(33);
                // Three seconds, not one: with a keyframe a second this is where the
                // one-fragment-per-keyframe contract becomes visible rather than assumed.
                let seconds = 3;
                for index in 0..(30 * seconds) {
                    let at = std::time::Instant::now();
                    match encoder.encode_texture(&texture, index * 33, std::time::Duration::from_secs(2)) {
                        Ok(bytes) => {
                            total += bytes.len();
                            stream.extend_from_slice(&bytes);
                            sink.write(&bytes)?;
                            frames += 1;
                        }
                        Err(err) => {
                            println!("frame {index} rejected: {err:#}");
                            break;
                        }
                    }
                    let spent = at.elapsed();
                    if spent < pace {
                        std::thread::sleep(pace - spent);
                    }
                }
                match encoder.finish(std::time::Duration::from_secs(2)) {
                    Ok(bytes) => total += bytes.len(),
                    Err(err) => println!("drain failed: {err:#}"),
                }
                println!("{frames} frames submitted, {total} bytes of H.264 out");
                println!(
                    "the stream starts {:02x?}",
                    &stream[..stream.len().min(8)]
                );
                // Step 6's question, asked rather than assumed. An H.264 elementary stream carries
                // no timestamps of its own, so what ffmpeg does with these bytes — and whether what
                // comes out is the fragmented MP4 the ring parses — is not something to guess at.
                println!("\n--- the bitstream into ffmpeg ---");
                match sink.finish() {
                    Ok(fmp4) => {
                        println!("ffmpeg produced {} bytes of fragmented MP4", fmp4.len());
                        match localplay_media::FragmentSplitter::new().push(&fmp4) {
                            Ok(fragments) => println!(
                                "the project's own splitter sees {} fragment(s): {:?}",
                                fragments.len(),
                                fragments
                                    .iter()
                                    .map(|f| (f.seq, f.start_ms, f.duration_ms, f.keyframe))
                                    .collect::<Vec<_>>()
                            ),
                            Err(err) => println!(
                                "the splitter could not read it, which is the finding: {err:#}"
                            ),
                        }
                    }
                    Err(err) => println!("ffmpeg refused it: {err:#}"),
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

/// An ffmpeg child taking an H.264 elementary stream on stdin and writing fragmented MP4 to stdout.
///
/// The command line is the project's own fragmented-MP4 shape — the one `EncodeConfig` uses for a
/// replay buffer, `empty_moov+frag_keyframe+default_base_moof` — because the question is not whether
/// ffmpeg can make *a* container but whether it can make *this* one out of bytes a hardware MFT
/// produced.
///
/// `-use_wallclock_as_timestamps 1` on the input is load-bearing: an elementary stream carries no
/// timestamps of its own, and this is what tells ffmpeg to take them from when the bytes arrive
/// rather than inventing a rigid grid — the lesson of issue #2, which cost an afternoon the first
/// time round. Which is also why this is a *stream* and not a batch: arrival times only mean
/// something if the bytes arrive when they were made.
#[cfg(windows)]
struct FfmpegSink {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    reading: std::thread::JoinHandle<Vec<u8>>,
}

#[cfg(windows)]
impl FfmpegSink {
    fn spawn() -> anyhow::Result<Self> {
        use anyhow::Context;
        use std::io::Read;
        use std::process::{Command, Stdio};

        let bin = localplay_media::FfmpegBinaries::discover(None).context("ffmpeg on PATH")?;
        let mut child = Command::new(&bin.ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "h264",
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
                "pipe:0",
                "-c:v",
                "copy",
                "-f",
                "mp4",
                "-movflags",
                "empty_moov+frag_keyframe+default_base_moof",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;

        // ffmpeg blocks on a full stdout, so the reading happens on its own thread or the writes
        // below would deadlock — the same reason the encoder's own reader thread exists.
        let mut stdout = child.stdout.take().context("ffmpeg's stdout")?;
        let reading = std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            out
        });
        let stdin = child.stdin.take().context("ffmpeg's stdin")?;
        Ok(Self {
            child,
            stdin,
            reading,
        })
    }

    /// Hand ffmpeg one access unit's worth of bitstream, as it is produced.
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::io::Write;
        if bytes.is_empty() {
            return Ok(());
        }
        self.stdin.write_all(bytes).context("writing the bitstream")?;
        // Flushed per access unit: buffering here would put the frames back in a batch, which is the
        // mistake this whole method exists to avoid.
        self.stdin.flush().context("flushing to ffmpeg")
    }

    /// Close the stream and collect what ffmpeg made of it.
    fn finish(self) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;
        use std::io::Read;

        let Self {
            mut child,
            stdin,
            reading,
        } = self;
        drop(stdin);
        let fmp4 = reading.join().expect("the reader thread");
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let status = child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            anyhow::bail!("ffmpeg exited {status}: {stderr}");
        }
        Ok(fmp4)
    }
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
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> anyhow::Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    use anyhow::Context;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_SAMPLE_DESC};

    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
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
