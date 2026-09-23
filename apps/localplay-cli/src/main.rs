//! localplay CLI — the Phase 1 headless replay-buffer PoC.

mod config;

use anyhow::{bail, Context, Result};
use config::Config;
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::probe::select_vendor;
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_events::hotkey::Hotkey;
use localplay_media::FfmpegBinaries;
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("buffer") => run_buffer(),
        Some(other) => bail!("unknown subcommand: {other}\nusage: localplay-cli buffer"),
        None => bail!("usage: localplay-cli buffer"),
    }
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}

fn run_buffer() -> Result<()> {
    let app_dir = app_data_dir();
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = Hotkey::parse(&cfg.hotkeys.clip)?;

    let scratch_dir = if cfg.buffer.scratch_dir.is_empty() {
        app_dir.join("scratch")
    } else {
        PathBuf::from(&cfg.buffer.scratch_dir)
    };
    let clips_dir = if cfg.storage.clips_dir.is_empty() {
        app_dir.join("clips")
    } else {
        PathBuf::from(&cfg.storage.clips_dir)
    };

    let buffer_cfg = BufferConfig {
        pre_ms: cfg.buffer.pre_seconds * 1000,
        post_ms: cfg.buffer.post_seconds * 1000,
        scratch_cap_bytes: cfg.buffer.scratch_cap_bytes,
        segment_ms: cfg.buffer.segment_time * 1000,
        clips_dir: clips_dir.clone(),
    };

    // `--dev-software-encoder` only exists when built with the test-encoders feature.
    let dev_software = std::env::args().any(|a| a == "--dev-software-encoder");
    let (mut encoder, encoder_name) = build_encoder(&bin, &cfg, &scratch_dir, dev_software)?;
    tracing::info!("encoding with {encoder_name}");

    // Phase 1 wires the stub sources so the pipeline runs on any platform. Tasks 14/15
    // swap these for WgcCapture + WasapiLoopback behind the same traits.
    let mut ring = RingBuffer::start(
        &bin,
        buffer_cfg.clone(),
        scratch_dir.clone(),
        encoder_name.clone(),
    )?;
    let adopted = ring.adopt_existing()?;
    if adopted > 0 {
        tracing::info!("adopted {adopted} segments from a previous run");
    }

    let (width, height) = parse_output_size(&cfg.encode.output_size, 1920, 1080);
    let mut capture: Box<dyn CaptureBackend> = Box::new(StubCapture::new(StubConfig {
        width,
        height,
        fps: cfg.encode.fps,
    }));
    let mut audio: Box<dyn AudioBackend> = Box::new(StubAudio::new(AudioFormat::default()));
    capture.start()?;
    audio.start()?;

    let hotkeys = localplay_events::hotkey::listen(hotkey)?;
    let clock = localplay_events::CaptureClock::new();
    tracing::info!(
        "buffering {}s pre / {}s post at {}fps; press {} to clip",
        cfg.buffer.pre_seconds,
        cfg.buffer.post_seconds,
        cfg.encode.fps,
        cfg.hotkeys.clip
    );

    // Capture loop. Frames go to the encoder; the ring scans for completed segments.
    let mut last_scan = Instant::now();
    loop {
        if let Some(frame) = capture.next_frame(Duration::from_millis(5))? {
            encoder.submit_video(&frame)?;
        }
        // Drain every audio block that is already due. Audio blocks are 10ms while
        // video frames are 16.7ms at 60fps, so submitting a single block per loop
        // iteration would run audio at ~60% speed and desync the clip.
        // A zero timeout makes `next_buffer` a non-blocking "is anything due?" check.
        while let Some(block) = audio.next_buffer(Duration::ZERO)? {
            encoder.submit_audio(&block)?;
        }

        if last_scan.elapsed() >= Duration::from_millis(200) {
            ring.scan_once().context("scanning scratch")?;
            ring.save_ledger()?;
            last_scan = Instant::now();

            let stats = ring.stats();
            tracing::debug!(
                "segments={} bytes={} span={}ms",
                stats.segments,
                stats.bytes_on_disk,
                stats.span_ms
            );
            if stats.bytes_on_disk > cfg.buffer.scratch_cap_bytes {
                bail!(
                    "scratch cap violated: {} bytes on disk exceeds {}",
                    stats.bytes_on_disk,
                    cfg.buffer.scratch_cap_bytes
                );
            }
        }

        if localplay_events::hotkey::wait_for_press(&hotkeys, Duration::from_millis(10)) {
            let trigger_ms = clock.ms_at(Instant::now());
            tracing::info!("hotkey pressed at {trigger_ms}ms; waiting for post-roll");

            // Wait for the post-roll to be written before splicing (spec §6.2 step 2).
            let need_ms = trigger_ms + buffer_cfg.post_ms;
            let deadline = Instant::now() + Duration::from_secs(5);
            while ring.stats().span_ms < need_ms {
                if Instant::now() > deadline {
                    bail!("timed out waiting for post-roll (span={}ms need={}ms)", ring.stats().span_ms, need_ms);
                }
                std::thread::sleep(Duration::from_millis(50));
                ring.scan_once()?;
            }

            let stem = format!("clip-{}", unix_seconds());
            let clip = ring.trigger(trigger_ms, &stem)?;
            tracing::info!(
                "wrote {} ({}ms, {} bytes, encoder={})",
                clip.path.display(),
                clip.duration_ms,
                clip.size_bytes,
                clip.encoder
            );
        }
    }
}

/// Build the encoder. Hardware is the only shipping path.
///
/// `dev_software` exists solely so the pipeline can be smoke-tested on a host with
/// no GPU encoder, and is only reachable when the CLI is built with
/// `--features test-encoders`. It is never reachable from the config file.
fn build_encoder(
    bin: &FfmpegBinaries,
    cfg: &Config,
    scratch_dir: &Path,
    dev_software: bool,
) -> Result<(Box<dyn Encoder>, String)> {
    let (width, height) = parse_output_size(&cfg.encode.output_size, 1920, 1080);
    let codec = match cfg.encode.codec.as_str() {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::Hevc,
        other => bail!("unsupported encode.codec: {other}"),
    };
    let segment_ms = cfg.buffer.segment_time * 1000;

    let encode_cfg = if dev_software {
        #[cfg(feature = "test-encoders")]
        {
            tracing::warn!(
                "--dev-software-encoder: using libx264. This is for smoke-testing the \
                 pipeline only and is NOT a supported configuration."
            );
            EncodeConfig::for_tests_software(
                codec,
                width,
                height,
                cfg.encode.fps,
                scratch_dir.to_path_buf(),
                segment_ms,
            )
        }
        #[cfg(not(feature = "test-encoders"))]
        {
            bail!("--dev-software-encoder requires building with `--features test-encoders`");
        }
    } else {
        let vendor = select_vendor(bin, &cfg.encode.vendor, codec)?;
        EncodeConfig::hardware(
            codec,
            vendor,
            width,
            height,
            cfg.encode.fps,
            cfg.encode.bitrate_kbps,
            segment_ms,
            scratch_dir.to_path_buf(),
        )
    };

    let encoder = FfmpegEncoder::spawn(bin, &encode_cfg)?;
    let name = encoder.active_encoder().to_string();
    Ok((Box::new(encoder), name))
}

/// `"1920x1080"`, or `""` for the supplied default.
fn parse_output_size(spec: &str, default_w: u32, default_h: u32) -> (u32, u32) {
    if spec.trim().is_empty() {
        return (default_w, default_h);
    }
    spec.split_once('x')
        .and_then(|(w, h)| {
            Some((
                w.trim().parse::<u32>().ok()?,
                h.trim().parse::<u32>().ok()?,
            ))
        })
        .unwrap_or((default_w, default_h))
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
