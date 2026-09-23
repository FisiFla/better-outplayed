//! The encoder probe: which hardware encoders this machine has, and — the part that
//! matters — which of them can actually encode a frame.
//!
//! Shared by `xtask probe` (prints it) and `xtask verify` (embeds it in the report and uses
//! it to pick a vendor that must fail, for criterion 7).

use anyhow::{Context, Result};
use localplay_media::FfmpegBinaries;
use std::path::PathBuf;
use std::process::Command;

/// Every hardware-encoder id the app knows about, in the order `vendor = "auto"` tries them.
pub const CANDIDATES: [&str; 6] =
    ["h264_nvenc", "hevc_nvenc", "h264_qsv", "hevc_qsv", "h264_amf", "hevc_amf"];

/// The vendors a config may name, paired with their H.264 encoder id.
pub const VENDORS: [(&str, &str); 3] =
    [("nvenc", "h264_nvenc"), ("qsv", "h264_qsv"), ("amf", "h264_amf")];

/// What the probe found about one encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// ffmpeg lists it *and* this machine encoded one 320x240 frame with it.
    Works,
    /// ffmpeg lists it; this machine could not open a session. Carries ffmpeg's reason.
    Fails(String),
    /// ffmpeg was not built with it.
    NotAdvertised,
}

impl State {
    /// The one-line form the runbook reads.
    pub fn describe(&self) -> String {
        match self {
            State::Works => "advertised, WORKS".to_string(),
            State::Fails(reason) => format!("advertised, FAILS: {reason}"),
            State::NotAdvertised => "not advertised".to_string(),
        }
    }
}

/// The probe's result: one row per candidate, plus the ffmpeg that answered.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub ffmpeg: PathBuf,
    pub rows: Vec<(String, State)>,
}

impl ProbeReport {
    /// Whether a given encoder id passed its smoke test.
    pub fn works(&self, encoder: &str) -> bool {
        self.rows
            .iter()
            .any(|(name, state)| name == encoder && *state == State::Works)
    }

    /// A vendor whose H.264 encoder this machine *cannot* use, for criterion 7.
    ///
    /// Prefers an `advertised, FAILS` vendor — the case that used to slip through, because
    /// the encoder list was the only check — and falls back to `not advertised`. `None`
    /// when every vendor works here, which is a legitimate machine state: the check is then
    /// reported as not performed rather than faked.
    pub fn unusable_h264_vendor(&self) -> Option<(&'static str, &'static str)> {
        let unusable = VENDORS.iter().filter(|(_, encoder)| !self.works(encoder));
        let advertised_fails = unusable
            .clone()
            .find(|(_, encoder)| self.state_of(encoder).is_some_and(|s| matches!(s, State::Fails(_))));
        advertised_fails.or_else(|| unusable.into_iter().next()).copied()
    }

    fn state_of(&self, encoder: &str) -> Option<&State> {
        self.rows.iter().find(|(name, _)| name == encoder).map(|(_, state)| state)
    }

    /// The text `xtask probe` prints (and the report embeds verbatim).
    pub fn render(&self) -> String {
        let mut text = format!("{}\n", self.ffmpeg.display());
        for (name, state) in &self.rows {
            text.push_str(&format!("{name:<12} {}\n", state.describe()));
        }
        text.push_str(
            "\nWORKS means ffmpeg listed it AND encoded one 320x240 frame with it. A vendor value \
             only works when it lands on WORKS. This is a synthetic frame: it does not capture \
             the screen and does not inject input.",
        );
        if self.rows.iter().all(|(_, s)| *s != State::Works) {
            text.push_str(
                "\nNo hardware encoder on this machine can encode a frame; the capture app will \
                 refuse to start rather than fall back to CPU encoding.",
            );
        }
        text
    }
}

/// Run the probe: `ffmpeg -encoders` for what the build carries, then a one-frame smoke
/// test for each advertised candidate.
pub fn probe(bin: &FfmpegBinaries) -> Result<ProbeReport> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .with_context(|| format!("running {}", bin.ffmpeg.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);

    let mut rows = Vec::with_capacity(CANDIDATES.len());
    for name in CANDIDATES {
        // Two questions, and only the second one is the answer. `-encoders` says the build
        // carries the encoder; the smoke test says *this machine* can run it, which is
        // what a `vendor` value in the config depends on. A vendor can advertise an
        // encoder it cannot initialise — measured on a box with no AMD hardware, ffmpeg
        // listed h264_amf and died with `DLL amfrt64.dll failed to open` when asked to use
        // it — so a row that says only "listed" is not enough to trust.
        let state = if !text.lines().any(|l| l.contains(name)) {
            State::NotAdvertised
        } else {
            match localplay_media::smoke_test_encoder(bin, name) {
                Ok(()) => State::Works,
                Err(reason) => State::Fails(reason),
            }
        };
        rows.push((name.to_string(), state));
    }
    Ok(ProbeReport { ffmpeg: bin.ffmpeg.clone(), rows })
}
