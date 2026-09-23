//! Choosing a hardware encoder, and failing loudly when there isn't one.

use crate::{VideoCodec, Vendor};
use anyhow::{bail, Context, Result};
use localplay_media::{smoke_test_encoder, FfmpegBinaries};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Vendors tried, in order, for `vendor = "auto"`.
const AUTO_ORDER: [Vendor; 3] = [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf];

/// Pick a hardware encoder, or explain precisely why none is usable.
///
/// There is no software fallback (spec §3.2). A CPU encode would silently destroy
/// in-game performance, so failing loudly is the correct behaviour.
///
/// A vendor is accepted only once it has **encoded a frame** ([`smoke_test_cached`]).
/// Being listed by `ffmpeg -encoders` is not evidence that anything can use it: that list
/// is what ffmpeg was compiled with, and it names `h264_amf` on a machine with no AMD
/// driver at all (measured: an RTX 3090 + Intel iGPU box, where the process then died
/// mid-capture with "video writer thread has stopped"). Asking ffmpeg to encode one frame
/// is what turns that into a startup failure that names the encoder and the reason.
pub fn select_vendor(bin: &FfmpegBinaries, requested: &str, codec: VideoCodec) -> Result<Vendor> {
    // Resolve the request before touching ffmpeg, so a bad config value fails
    // immediately and does not depend on the machine's hardware.
    let candidates = candidates_for(requested)?;

    let advertised = advertised_encoders(bin)?;
    select_usable(&candidates, codec, &advertised, |encoder| {
        smoke_test_cached(bin, encoder)
    })
}

/// The vendors a config value asks for, in the order they are tried.
fn candidates_for(requested: &str) -> Result<Vec<Vendor>> {
    Ok(match requested {
        "auto" => AUTO_ORDER.to_vec(),
        "nvenc" => vec![Vendor::Nvenc],
        "qsv" => vec![Vendor::Qsv],
        "amf" => vec![Vendor::Amf],
        other => bail!("unknown encode.vendor: {other}"),
    })
}

/// The selection loop, with "can this machine actually run it?" injected.
///
/// `usable` is a parameter rather than a direct call into ffmpeg so that the ordering and
/// the reporting rules below are testable on a host with no hardware encoder at all: every
/// GPU encoder is absent from the development host's ffmpeg, so a hard-coded call could
/// only ever be exercised on the Windows box itself. The real probe is
/// [`smoke_test_encoder`], passed in by [`select_vendor`].
///
/// A candidate that is not advertised is never probed — there is nothing to probe — and a
/// candidate that *is* advertised still has to encode a frame before it is chosen. The
/// first candidate that passes wins; if none does, the error names every candidate tried
/// and what happened to each one.
fn select_usable(
    candidates: &[Vendor],
    codec: VideoCodec,
    advertised: &[String],
    mut usable: impl FnMut(&'static str) -> Result<(), String>,
) -> Result<Vendor> {
    let mut outcome: Vec<String> = Vec::with_capacity(candidates.len());
    for vendor in candidates {
        let encoder = codec.hw_encoder_name(*vendor);
        if !advertised.iter().any(|name| name == encoder) {
            outcome.push(format!("{encoder}: not advertised by this ffmpeg build"));
            continue;
        }
        match usable(encoder) {
            Ok(()) => {
                // A breadcrumb for "why did startup spend a moment here": the vendor that
                // was chosen is the one that encoded a frame, not the first one listed.
                tracing::debug!("{encoder} passed its smoke test");
                return Ok(*vendor);
            }
            Err(reason) => outcome.push(format!(
                "{encoder}: advertised, but it could not encode a single frame: {reason}"
            )),
        }
    }

    bail!(
        "no usable hardware encoder. Tried in order — {}. \
         An encoder appearing in `ffmpeg -encoders` does not mean this machine can run \
         it: that list names every encoder ffmpeg was built with, including ones whose \
         vendor runtime is not installed. Install the GPU vendor runtime (NVIDIA driver / \
         Intel graphics driver / AMD Adrenalin), or set encode.vendor to a vendor this \
         machine has. localplay will not fall back to CPU encoding because it would cost \
         game performance.",
        outcome.join("; ")
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

/// What a smoke test said: whether the encoder encoded a frame, or ffmpeg's reason why not.
type SmokeResult = Result<(), String>;

/// Encoder name → smoke-test result. A `Mutex` rather than a lock-free map because a
/// probe runs once per encoder per process, not in a hot loop.
type SmokeCache = Mutex<Vec<(&'static str, SmokeResult)>>;

/// Smoke-test results for this process, keyed by encoder name.
///
/// Startup must not pay for the same ffmpeg child twice. Whether this machine can
/// initialise a given encoder cannot change while the process runs, so the answers are
/// memoised for the life of the process. The key is the encoder name alone: one process
/// resolves one ffmpeg binary, and `FfmpegBinaries` is fixed for the length of a run.
static SMOKE_RESULTS: OnceLock<SmokeCache> = OnceLock::new();

/// [`smoke_test_encoder`], memoised for the process lifetime.
///
/// The lock is never held across the child: a probe can take seconds, and nothing else
/// should wait on it. A poisoned lock is recovered rather than panicking on the failure
/// path — the worst case is that one probe is run twice.
fn smoke_test_cached(bin: &FfmpegBinaries, encoder: &'static str) -> Result<(), String> {
    let results = SMOKE_RESULTS.get_or_init(|| Mutex::new(Vec::new()));

    let cached = results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|(name, _)| *name == encoder)
        .map(|(_, result)| result.clone());
    if let Some(result) = cached {
        return result;
    }

    let result = smoke_test_encoder(bin, encoder);
    results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((encoder, result.clone()));
    result
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

    #[test]
    fn a_named_vendor_is_the_only_candidate_tried() {
        assert_eq!(candidates_for("amf").unwrap(), vec![Vendor::Amf]);
        assert_eq!(candidates_for("qsv").unwrap(), vec![Vendor::Qsv]);
        assert_eq!(candidates_for("nvenc").unwrap(), vec![Vendor::Nvenc]);
        assert_eq!(candidates_for("auto").unwrap(), AUTO_ORDER.to_vec());
    }

    /// The bug this module's smoke test exists for: ffmpeg advertises an encoder the
    /// machine cannot initialise. Selection must reject it and use the next candidate.
    #[test]
    fn an_advertised_but_unusable_encoder_is_rejected_in_favour_of_the_next_candidate() {
        let advertised = advertised_all_h264();
        let mut probed: Vec<&str> = Vec::new();

        let vendor = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &advertised,
            |encoder| {
                probed.push(encoder);
                match encoder {
                    // The measured Windows shape: the encoder is there to be listed, and
                    // cannot start because its vendor runtime is not installed.
                    "h264_nvenc" => Err("Cannot load nvcuda.dll".to_string()),
                    _ => Ok(()),
                }
            },
        )
        .expect("h264_qsv passes its smoke test");

        assert_eq!(vendor, Vendor::Qsv, "the first vendor that can encode wins");
        assert_eq!(
            probed,
            ["h264_nvenc", "h264_qsv"],
            "candidates are tried in order and the loop stops at the first pass"
        );
    }

    /// A named vendor must never be quietly substituted: `vendor = "amf"` asks for AMF.
    #[test]
    fn a_requested_vendor_that_fails_is_an_error_not_a_substitution() {
        let err = select_usable(
            &candidates_for("amf").unwrap(),
            VideoCodec::H264,
            &advertised_all_h264(),
            |_| Err("DLL amfrt64.dll failed to open".to_string()),
        )
        .expect_err("amf was requested and amf cannot encode");

        let msg = err.to_string();
        assert!(msg.contains("h264_amf"), "the failure names the encoder: {msg}");
        assert!(msg.contains("DLL amfrt64.dll failed to open"), "and ffmpeg's reason: {msg}");
        assert!(
            !msg.contains("h264_nvenc") && !msg.contains("h264_qsv"),
            "no other vendor was even considered: {msg}"
        );
    }

    /// The message is the deliverable: it has to name every candidate and its own reason.
    #[test]
    fn a_failed_selection_names_every_vendor_tried_and_its_concrete_reason() {
        // Only AMF is advertised, so the other two report the other failure mode.
        let advertised = vec!["h264_amf".to_string()];
        let err = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &advertised,
            |encoder| Err(format!("{encoder}: cannot load its vendor library")),
        )
        .expect_err("nothing can encode here");

        let msg = err.to_string();
        for name in ["h264_nvenc", "h264_qsv", "h264_amf"] {
            assert!(msg.contains(name), "{name} must be named: {msg}");
        }
        assert!(msg.contains("not advertised"), "the absent ones say why: {msg}");
        assert!(
            msg.contains("h264_amf: advertised, but it could not encode a single frame"),
            "the advertised one reports its own failure: {msg}"
        );
        assert!(
            msg.contains("h264_amf: cannot load its vendor library"),
            "and carries ffmpeg's concrete reason: {msg}"
        );
        assert!(
            msg.contains("will not fall back to CPU encoding")
                && msg.contains("game performance"),
            "the no-CPU-fallback guarantee and its reason must survive: {msg}"
        );
    }

    /// An encoder that is not advertised is not probed at all — the list is still the
    /// cheap first filter, and probing an encoder ffmpeg does not know would report
    /// "unknown encoder" instead of the real story.
    #[test]
    fn a_candidate_that_is_not_advertised_is_never_probed() {
        let mut probed: Vec<&str> = Vec::new();
        let err = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &[],
            |encoder| {
                probed.push(encoder);
                Ok(())
            },
        )
        .expect_err("an empty advertised list cannot produce a vendor");

        assert!(probed.is_empty(), "nothing was spawned: {probed:?}");
        assert!(err.to_string().contains("h264_nvenc"), "got: {err}");
    }

    /// The whole point of the smoke test: `h264_amf` is in ffmpeg's list on a box with no
    /// AMD driver, so *advertising is not acceptance*.
    #[test]
    fn advertising_alone_does_not_make_a_vendor_usable() {
        let advertised = vec!["h264_amf".to_string()];
        let err = select_usable(
            &candidates_for("amf").unwrap(),
            VideoCodec::H264,
            &advertised,
            |_| Err("DLL amfrt64.dll failed to open".to_string()),
        )
        .expect_err("being listed is not enough; it has to encode a frame");

        assert!(err.to_string().contains("could not encode a single frame"), "got: {err}");
    }

    fn advertised_all_h264() -> Vec<String> {
        ["h264_nvenc", "h264_qsv", "h264_amf"].iter().map(|s| s.to_string()).collect()
    }
}
