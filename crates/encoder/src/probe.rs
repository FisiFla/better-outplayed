//! Choosing a hardware encoder, and failing loudly when there isn't one.

use crate::{VideoCodec, Vendor};
use anyhow::{bail, Context, Result};
use localplay_media::FfmpegBinaries;
use std::process::Command;

/// Vendors tried, in order, for `vendor = "auto"`.
const AUTO_ORDER: [Vendor; 3] = [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf];

/// Pick a hardware encoder, or explain precisely why none is usable.
///
/// There is no software fallback (spec §3.2). A CPU encode would silently destroy
/// in-game performance, so failing loudly is the correct behaviour.
pub fn select_vendor(bin: &FfmpegBinaries, requested: &str, codec: VideoCodec) -> Result<Vendor> {
    // Resolve the request before touching ffmpeg, so a bad config value fails
    // immediately and does not depend on the machine's hardware.
    let candidates: Vec<Vendor> = match requested {
        "auto" => AUTO_ORDER.to_vec(),
        "nvenc" => vec![Vendor::Nvenc],
        "qsv" => vec![Vendor::Qsv],
        "amf" => vec![Vendor::Amf],
        other => bail!("unknown encode.vendor: {other}"),
    };

    let advertised = advertised_encoders(bin)?;
    if let Some(vendor) = candidates
        .iter()
        .find(|v| advertised.iter().any(|e| *e == codec.hw_encoder_name(**v)))
    {
        return Ok(*vendor);
    }

    let wanted: Vec<&str> = candidates.iter().map(|v| codec.hw_encoder_name(*v)).collect();
    bail!(
        "no usable hardware encoder. ffmpeg advertises none of: {}. \
         Install the GPU vendor runtime (NVIDIA driver / Intel graphics driver / \
         AMD Adrenalin), or set encode.vendor to a vendor this machine has. \
         localplay will not fall back to CPU encoding because it would cost game performance.",
        wanted.join(", ")
    )
}

/// Encoder names ffmpeg advertises. `-encoders` lines look like:
/// ` V....D h264_nvenc  NVIDIA NVENC H.264 encoder (codec h264)`.
fn advertised_encoders(bin: &FfmpegBinaries) -> Result<Vec<String>> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .with_context(|| format!("running {}", bin.ffmpeg.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_vendor_name_fails_without_probing_ffmpeg() {
        // A bogus binary path proves validation happens before any spawn.
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        let err = select_vendor(&bin, "voodoo", VideoCodec::H264).unwrap_err();
        assert!(err.to_string().contains("voodoo"), "got: {err}");
    }
}
