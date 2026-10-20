//! The encoder panel's settings: what the encoder is asked for, and what it can do.
//!
//! # The design is OBS Studio's
//!
//! Three lessons from the Output → Encoder tab, applied here:
//!
//! 1. **The panel shows what the selected encoder supports**, not what an encoder might support
//!    in general. That is why [`encoder_status`] returns [`EncoderCapabilities`]: the rate-control
//!    modes and the presets a list may offer depend on the *path* the recording will take, and
//!    the path is not the vendor.
//! 2. **The encoder that was actually chosen is named**, not a bare "auto". The startup smoke
//!    test picks one, and the panel says which.
//! 3. **CBR / VBR / constant quality** as the rate-control choice, and a per-encoder preset
//!    list beside it.
//!
//! # What the panel must not be told
//!
//! The zero-copy Media Foundation path and the ffmpeg-child path support different subsets, and
//! the difference is not a detail:
//!
//! | | zero-copy (Media Foundation) | ffmpeg child |
//! |---|---|---|
//! | rate control | CBR, VBR | CBR, VBR, constant quality |
//! | presets | none (`ICodecAPI` has no such key) | the vendor's own (`p1`…`p7`, `veryfast`…, `speed`…) |
//! | scaling | **no** — an MFT encodes what it is given | yes (`-vf scale`) |
//!
//! So the capabilities are computed from the path *and* the vendor, and the panel is expected to
//! hide what the current path does not have. See [`EncoderPath`].
//!
//! The honest consequence, which the panel's copy has to carry: **a scaled output needs the
//! ffmpeg-child path.** The Media Foundation path cannot scale, and a recording that asks for
//! both is refused at spawn (see `replayed_encoder::ffmpeg::FfmpegEncoder::spawn`).

use super::*;
use crate::config::{self, MAX_AUDIO_BITRATE_KBPS, MAX_BITRATE_KBPS};
use replayed_encoder::{EncoderFamily, RateControl, VideoCodec};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------------------
// The types the panel sees
// ---------------------------------------------------------------------------------------

/// The encoder settings the panel shows and writes.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the exact
/// JSON keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EncoderSettings {
    /// `[encode] vendor`: `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`.
    pub vendor: String,
    /// `[encode] codec`: `"h264"` or `"hevc"`.
    pub codec: String,
    /// `[encode] bitrate_kbps`: the video rate, in kilobits per second.
    pub bitrate_kbps: u32,
    /// `[encode] fps`: a **ceiling**, not a promise — the recorder measures what the machine can
    /// sustain and records at the lower of that and this.
    pub fps: u32,
    /// `[encode] output_size`: `""` is the capture's own size, else `"WIDTHxHEIGHT"`.
    pub output_size: String,
    /// `[encode] adapt_fps`: measure the sustainable rate at startup and record at the lower of
    /// that and `fps`.
    pub adapt_fps: bool,
    /// `[encode] zero_copy`: `null` lets the recorder decide (the hardware path when the
    /// machine has one), `true` requires it, `false` turns it off.
    pub zero_copy: Option<bool>,
    /// `[encode] rate_control`: `"cbr"`, `"vbr"` or `"quality"`.
    pub rate_control: String,
    /// `[encode] preset`: the encoder's own preset, `""` for its default.
    pub preset: String,
    /// `[audio] bitrate_kbps`: the game-audio track's rate.
    pub audio_bitrate_kbps: u32,
}

/// What the encoder that will run can do.
///
/// **Per path, not per vendor** — see the module comment for the table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EncoderCapabilities {
    /// The rate-control modes the panel may offer: `"cbr"`, `"vbr"` and/or `"quality"`.
    pub rate_control_modes: Vec<String>,
    /// The presets the panel may offer, empty when the path has none.
    pub presets: Vec<String>,
    /// Whether the path can record at a size other than the capture's.
    pub supports_scaling: bool,
    /// The highest rate the panel will write.
    pub max_bitrate_kbps: u32,
}

/// What the encoder is and what it can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EncoderStatus {
    /// What `[encode] vendor` asks for: `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`.
    pub requested_vendor: String,
    /// The vendor the startup smoke test chose, `null` before a recording has run.
    pub selected_vendor: Option<String>,
    /// The codec the last recording ran, `null` before a recording has run.
    pub selected_codec: Option<String>,
    /// What the encoder that will run can do.
    pub capabilities: EncoderCapabilities,
    /// A sentence for the panel: which encoder was chosen, and what the path can and cannot do.
    pub message: String,
}

// ---------------------------------------------------------------------------------------
// The path
// ---------------------------------------------------------------------------------------

/// Which encoder path a recording takes.
///
/// **The path, not the vendor, decides what the encoder can do.** The zero-copy path hands the
/// captured texture to the hardware encoder and lets ffmpeg copy what comes out; the
/// ffmpeg-child path lets ffmpeg scale and encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPath {
    /// The zero-copy Media Foundation path: the encoder takes the texture, ffmpeg copies.
    ///
    /// It cannot scale (an MFT encodes what it is given, and ffmpeg is only copying), it has no
    /// preset, and its rate control reaches constant and variable bitrate.
    ZeroCopy,
    /// The ffmpeg-child path: ffmpeg scales (`-vf scale`) and encodes with the vendor's own
    /// switch, and the vendor's presets apply.
    FfmpegChild,
    /// Undecided: `encode.zero_copy` is unset, so the path is chosen at the next recording
    /// start, when the machine's own encoder is probed.
    ///
    /// The capabilities reported for this are the ones **both** paths share, so the panel never
    /// offers something that may not work.
    Undecided,
}

impl EncoderPath {
    /// The path a configuration's `zero_copy` asks for.
    pub fn of(zero_copy: Option<bool>) -> Self {
        match zero_copy {
            Some(true) => EncoderPath::ZeroCopy,
            Some(false) => EncoderPath::FfmpegChild,
            None => EncoderPath::Undecided,
        }
    }

    /// A sentence for the panel.
    fn describe(self) -> &'static str {
        match self {
            EncoderPath::ZeroCopy => {
                "the zero-copy Media Foundation path: the encoder takes the captured texture \
                 and ffmpeg copies the result, so it cannot scale the picture, has no preset, \
                 and rate-controls as constant or variable bitrate"
            }
            EncoderPath::FfmpegChild => {
                "the ffmpeg child path: ffmpeg scales the picture and encodes with the \
                 vendor's own settings, so the vendor's presets and the constant-quality mode \
                 are available"
            }
            EncoderPath::Undecided => {
                "encode.zero_copy is unset, so the path is decided at the next recording start \
                 (the Media Foundation path when this machine has an encoder that opens for \
                 it): the capabilities below are the ones both paths share"
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------------------

/// The effective encoder settings, for the panel.
///
/// The file's values where it sets them, the example's where it does not.
pub fn encoder_settings(config_path: &Path) -> Result<EncoderSettings, CommandError> {
    config::read_encoder_settings(config_path)
}

/// Validate the settings, write them, and report what was actually stored.
///
/// The answer is read back from the file rather than echoed: what was stored is what the file
/// says, after the defaults have been filled in. A validation failure writes nothing.
pub fn update_encoder_settings(
    config_path: &Path,
    settings: &EncoderSettings,
) -> Result<EncoderSettings, CommandError> {
    validate(settings)?;
    // The write and the read back are one sequence: the panel is told what was stored, and that
    // answer has to come from the file the write produced.
    let _guard = config::settings_write_lock();
    config::write_encoder_settings_locked(config_path, settings)?;
    config::read_encoder_settings(config_path)
}

/// Which encoder was chosen, and what it can do.
///
/// `last_encoder` is the encoder the last recording ran (`h264_nvenc`), or `None` before one has:
/// the smoke test that picks the vendor runs at the start of a recording, so there is nothing to
/// report before then.
pub fn encoder_status(
    config_path: &Path,
    last_encoder: Option<&str>,
) -> Result<EncoderStatus, CommandError> {
    let settings = config::read_encoder_settings(config_path)?;
    let path = EncoderPath::of(settings.zero_copy);
    let selected = last_encoder.map(|name| (name, EncoderFamily::of(name)));
    let selected_vendor = selected
        .as_ref()
        .and_then(|(_, family)| family.vendor())
        .map(|vendor| vendor.as_str().to_string());
    let selected_codec = selected
        .as_ref()
        .and_then(|(name, _)| VideoCodec::of_encoder(name))
        .map(|codec| codec.as_str().to_string()); // The vendor the capabilities are for: the one that was chosen, else the one that was asked
                                                  // for (when it is not "auto", in which case the choice is not known yet).
    let vendor = selected_vendor
        .clone()
        .or_else(|| match settings.vendor.as_str() {
            "auto" => None,
            other => Some(other.to_string()),
        });
    let capabilities = capabilities_for(vendor.as_deref(), path);
    let message = status_message(&settings, last_encoder, &selected_vendor, path);
    Ok(EncoderStatus {
        requested_vendor: settings.vendor.clone(),
        selected_vendor,
        selected_codec,
        capabilities,
        message,
    })
}

/// What the encoder that will run can do.
///
/// The rules, one by one:
///
/// * **The zero-copy path** offers constant and variable bitrate and **no** preset: the MFT's
///   rate-control switch has no constant-quality mode this app can reach (it needs a QP value
///   the app does not carry), and `ICodecAPI` has no preset key at all.
/// * **The ffmpeg-child path** offers every mode and the vendor's presets.
/// * **An undecided path** offers what both paths share, so the panel never offers something
///   that may not work on the path the next recording picks.
pub fn capabilities_for(vendor: Option<&str>, path: EncoderPath) -> EncoderCapabilities {
    let modes: Vec<String> = match path {
        EncoderPath::FfmpegChild => RateControl::ALL
            .iter()
            .map(|mode| mode.as_str().to_string())
            .collect(),
        EncoderPath::ZeroCopy | EncoderPath::Undecided => replayed_encoder::MFT_RATE_CONTROLS
            .iter()
            .map(|mode| mode.as_str().to_string())
            .collect(),
    };
    let presets: Vec<String> = match path {
        EncoderPath::FfmpegChild => match vendor.and_then(EncoderFamily::of_vendor) {
            Some(family) => family.presets().iter().map(|p| p.to_string()).collect(),
            // "auto": any of the three may be selected, so the union is what may be offered.
            None => [EncoderFamily::Nvenc, EncoderFamily::Qsv, EncoderFamily::Amf]
                .into_iter()
                .flat_map(|family| family.presets().iter().map(|p| p.to_string()))
                .collect(),
        },
        // The zero-copy path has no preset, and an undecided path may land on it.
        EncoderPath::ZeroCopy | EncoderPath::Undecided => Vec::new(),
    };
    EncoderCapabilities {
        rate_control_modes: modes,
        presets,
        // Scaling needs a decoder, and the zero-copy path is the one that keeps the decoder out
        // of the process.
        supports_scaling: path == EncoderPath::FfmpegChild,
        max_bitrate_kbps: MAX_BITRATE_KBPS,
    }
}

/// The sentence the panel shows under the encoder's name.
fn status_message(
    settings: &EncoderSettings,
    last_encoder: Option<&str>,
    selected_vendor: &Option<String>,
    path: EncoderPath,
) -> String {
    let mut message = match (last_encoder, selected_vendor) {
        (Some(name), Some(vendor)) => format!(
            "the last recording used {name}: the startup smoke test chose {vendor}.",
            name = name,
            vendor = vendor
        ),
        (Some(name), None) => format!("the last recording used {name}."),
        (None, _) => format!(
            "no recording has run yet, so no encoder has been chosen: encode.vendor = {} picks \
             the first vendor that can encode a frame.",
            settings.vendor
        ),
    };
    message.push(' ');
    message.push_str(path.describe());
    message.push('.');
    // The one combination that cannot work, said where the user will read it.
    if !settings.output_size.trim().is_empty() && path != EncoderPath::FfmpegChild {
        message.push_str(
            " A scaled output needs the ffmpeg child path: the Media Foundation path cannot \
             scale, so a recording that asks for both is refused at start.",
        );
    }
    message
}

// ---------------------------------------------------------------------------------------
// The write path's rules
// ---------------------------------------------------------------------------------------

/// What the panel refuses to write.
///
/// **The panel may be stricter than the engine** (see `config.rs`): every value refused here has
/// already rendered in the panel, so a refusal is a helpful "not that" rather than a lockout.
/// The engine's own rules still apply on top of these.
fn validate(settings: &EncoderSettings) -> Result<(), CommandError> {
    if !["auto", "nvenc", "qsv", "amf"].contains(&settings.vendor.as_str()) {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.vendor is {:?}, but it must be \"auto\" (the first vendor that can encode \
                 a frame), \"nvenc\", \"qsv\" or \"amf\"",
                settings.vendor
            ),
        ));
    }
    if !["h264", "hevc"].contains(&settings.codec.as_str()) {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.codec is {:?}, but it must be \"h264\" or \"hevc\"",
                settings.codec
            ),
        ));
    }
    if settings.bitrate_kbps == 0 || settings.bitrate_kbps > MAX_BITRATE_KBPS {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.bitrate_kbps is {}, but the rate must be between 1 and {MAX_BITRATE_KBPS} \
                 kbit/s",
                settings.bitrate_kbps
            ),
        ));
    }
    if settings.audio_bitrate_kbps == 0 || settings.audio_bitrate_kbps > MAX_AUDIO_BITRATE_KBPS {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "audio.bitrate_kbps is {}, but the rate must be between 1 and \
                 {MAX_AUDIO_BITRATE_KBPS} kbit/s",
                settings.audio_bitrate_kbps
            ),
        ));
    }
    if !(config::FPS_MIN..=config::FPS_MAX).contains(&settings.fps) {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.fps is {}, but the capture rate must be between {} and {}",
                settings.fps,
                config::FPS_MIN,
                config::FPS_MAX
            ),
        ));
    }
    // The engine's own parser, so the panel refuses exactly what a start refuses.
    replayed_recorder::parse_output_size(&settings.output_size).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("encode.output_size is not usable: {err:#}"),
        )
    })?;
    // The mode, with the message the encoder gives: an unknown one names the three that exist.
    RateControl::parse(&settings.rate_control).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("encode.rate_control is not usable: {err:#}"),
        )
    })?;
    validate_preset(&settings.vendor, &settings.preset)?;
    // The one combination that cannot work at all: the Media Foundation path cannot scale, and a
    // recording that asks for both is refused at spawn. Refused here, where the user's hand is
    // still on the value, instead of at the next start.
    if settings.zero_copy == Some(true) && !settings.output_size.trim().is_empty() {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            "encode.output_size cannot be set while encode.zero_copy is on: the zero-copy Media \
             Foundation path cannot scale the picture, and a recording that asks for both is \
             refused at start. Set encode.zero_copy = false to record at another size.",
        ));
    }
    Ok(())
}

/// The preset, against the vendor that will be asked to use it.
///
/// With `vendor = "auto"` any of the three may be selected, so the union is what is accepted:
/// a preset that belongs to another family is dropped by the encoder with a warning, and the
/// panel's list for the chosen encoder is what the user should pick from.
fn validate_preset(vendor: &str, preset: &str) -> Result<(), CommandError> {
    if preset.is_empty() {
        return Ok(());
    }
    let families: Vec<EncoderFamily> = match vendor {
        "nvenc" => vec![EncoderFamily::Nvenc],
        "qsv" => vec![EncoderFamily::Qsv],
        "amf" => vec![EncoderFamily::Amf],
        _ => vec![EncoderFamily::Nvenc, EncoderFamily::Qsv, EncoderFamily::Amf],
    };
    if families
        .iter()
        .any(|family| family.presets().contains(&preset))
    {
        return Ok(());
    }
    let offered: Vec<&str> = families
        .iter()
        .flat_map(|family| family.presets().iter().copied())
        .collect();
    Err(CommandError::new(
        ErrorCode::InvalidInput,
        format!(
            "encode.preset is {preset:?}, which the {vendor} encoder does not take. It takes \
             {offered:?}."
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings a fresh install has: the example's, as the example's file says.
    fn settings() -> EncoderSettings {
        EncoderSettings {
            vendor: "auto".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 20_000,
            fps: 60,
            output_size: String::new(),
            adapt_fps: true,
            zero_copy: None,
            rate_control: "cbr".to_string(),
            preset: String::new(),
            audio_bitrate_kbps: 192,
        }
    }

    /// A config file with the given `[encode]` body.
    fn config_with(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    // -- the JSON contract ---------------------------------------------------------------

    #[test]
    fn the_encoder_dto_json_matches_the_typescript_interface() {
        // The frontend mirrors these three structs by hand; a rename on either side is found
        // here, not by a settings panel that silently shows nothing.
        let value = serde_json::to_value(settings()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "adapt_fps",
                "audio_bitrate_kbps",
                "bitrate_kbps",
                "codec",
                "fps",
                "output_size",
                "preset",
                "rate_control",
                "vendor",
                "zero_copy"
            ]
        );

        let capabilities =
            serde_json::to_value(capabilities_for(Some("nvenc"), EncoderPath::FfmpegChild))
                .unwrap();
        let mut keys: Vec<&str> = capabilities
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "max_bitrate_kbps",
                "presets",
                "rate_control_modes",
                "supports_scaling"
            ]
        );

        let status = serde_json::to_value(EncoderStatus {
            requested_vendor: "auto".to_string(),
            selected_vendor: Some("nvenc".to_string()),
            selected_codec: Some("h264".to_string()),
            capabilities: capabilities_for(Some("nvenc"), EncoderPath::ZeroCopy),
            message: "x".to_string(),
        })
        .unwrap();
        let mut keys: Vec<&str> = status
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "capabilities",
                "message",
                "requested_vendor",
                "selected_codec",
                "selected_vendor"
            ]
        );
    }

    // -- the capabilities ----------------------------------------------------------------

    /// The zero-copy path cannot scale, has no preset and reaches two rate-control modes.
    ///
    /// These are the three things the Media Foundation path genuinely cannot do, and a panel
    /// that offered them would offer a recording that does not start.
    #[test]
    fn the_zero_copy_path_offers_no_scaling_no_presets_and_two_modes() {
        let caps = capabilities_for(Some("nvenc"), EncoderPath::ZeroCopy);
        assert!(!caps.supports_scaling, "an MFT encodes what it is given");
        assert!(caps.presets.is_empty(), "ICodecAPI has no preset key");
        assert_eq!(caps.rate_control_modes, vec!["cbr", "vbr"]);
    }

    /// The ffmpeg child path can do all three, and takes the vendor's presets.
    #[test]
    fn the_ffmpeg_child_path_offers_every_mode_and_the_vendors_presets() {
        let caps = capabilities_for(Some("nvenc"), EncoderPath::FfmpegChild);
        assert!(caps.supports_scaling, "ffmpeg scales with -vf scale");
        assert_eq!(caps.rate_control_modes, vec!["cbr", "vbr", "quality"]);
        assert!(caps.presets.contains(&"p4".to_string()));

        let qsv = capabilities_for(Some("qsv"), EncoderPath::FfmpegChild);
        assert!(qsv.presets.contains(&"medium".to_string()));
        assert!(!qsv.presets.contains(&"p4".to_string()));

        let amf = capabilities_for(Some("amf"), EncoderPath::FfmpegChild);
        assert!(amf.presets.contains(&"balanced".to_string()));
    }

    /// An undecided path offers what both paths share, so the panel never offers something that
    /// may not work on the path the next recording picks.
    #[test]
    fn an_undecided_path_offers_only_what_both_paths_share() {
        let caps = capabilities_for(Some("nvenc"), EncoderPath::Undecided);
        assert!(!caps.supports_scaling);
        assert!(caps.presets.is_empty());
        assert_eq!(caps.rate_control_modes, vec!["cbr", "vbr"]);
    }

    /// With "auto" the vendor is not known, so the union of the three lists is what may be
    /// offered.
    #[test]
    fn an_unknown_vendor_offers_the_union_of_the_presets() {
        let caps = capabilities_for(None, EncoderPath::FfmpegChild);
        assert!(caps.presets.contains(&"p4".to_string()));
        assert!(caps.presets.contains(&"medium".to_string()));
        assert!(caps.presets.contains(&"balanced".to_string()));
    }

    // -- the write path ------------------------------------------------------------------

    /// The settings are written to the file and read back.
    #[test]
    fn the_settings_are_written_and_read_back() {
        let (_dir, path) = config_with("[storage]\nclips_dir = \"\"\n");
        let mut wanted = settings();
        wanted.vendor = "qsv".to_string();
        wanted.codec = "hevc".to_string();
        wanted.bitrate_kbps = 12_345;
        wanted.fps = 30;
        wanted.output_size = "1280x720".to_string();
        wanted.adapt_fps = false;
        wanted.zero_copy = Some(false);
        wanted.rate_control = "vbr".to_string();
        wanted.preset = "medium".to_string();
        wanted.audio_bitrate_kbps = 128;

        let stored = update_encoder_settings(&path, &wanted).unwrap();

        assert_eq!(stored, wanted, "what was stored is what was sent");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("vendor = \"qsv\""), "{text}");
        assert!(text.contains("rate_control = \"vbr\""), "{text}");
        assert!(text.contains("preset = \"medium\""), "{text}");
        assert!(text.contains("bitrate_kbps = 12345"), "{text}");
        assert!(
            text.contains("audio.bitrate_kbps") || text.contains("bitrate_kbps = 128"),
            "{text}"
        );

        // And the file reads back through the reader, which is the panel's own view of it.
        assert_eq!(encoder_settings(&path).unwrap(), wanted);
    }

    /// A rejected setting writes nothing.
    #[test]
    fn a_rejected_setting_writes_nothing() {
        let (_dir, path) = config_with("[encode]\nvendor = \"auto\"\n");
        let before = std::fs::read_to_string(&path).unwrap();
        let mut wanted = settings();
        wanted.rate_control = "crf".to_string();

        let err = update_encoder_settings(&path, &wanted).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("rate_control"),
            "the message names the key: {}",
            err.message
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// The preset is refused when the vendor does not take it.
    #[test]
    fn a_preset_the_vendor_does_not_take_is_refused() {
        let mut wanted = settings();
        wanted.vendor = "qsv".to_string();
        wanted.preset = "p4".to_string();
        let err = validate(&wanted).unwrap_err();
        assert!(err.message.contains("p4"), "got: {}", err.message);
        assert!(
            err.message.contains("medium"),
            "and what it does take: {}",
            err.message
        );

        // The same preset is fine for the vendor that takes it.
        wanted.vendor = "nvenc".to_string();
        assert!(validate(&wanted).is_ok());
    }

    /// A rate-control mode the encoder cannot be told to use is refused.
    ///
    /// "crf" is the one a user will reach for: it is what every other tool calls it. It is a
    /// software-encoder mode, and this app has no software encoder — so it is refused with the
    /// modes that exist, rather than written and then ignored by the encoder.
    #[test]
    fn a_rate_control_the_encoder_cannot_be_told_is_refused() {
        let mut wanted = settings();
        wanted.rate_control = "crf".to_string();
        let err = validate(&wanted).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("crf"),
            "the message names the value: {}",
            err.message
        );
        assert!(
            err.message.contains("quality"),
            "and the mode that is closest to it: {}",
            err.message
        );

        // The three that exist are accepted.
        for mode in ["cbr", "vbr", "quality"] {
            wanted.rate_control = mode.to_string();
            assert!(validate(&wanted).is_ok(), "{mode}");
        }
    }

    /// A scaled output and the zero-copy path cannot both be asked for: the recording would be
    /// refused at spawn.
    #[test]
    fn a_scaled_output_with_the_zero_copy_path_is_refused() {
        let mut wanted = settings();
        wanted.zero_copy = Some(true);
        wanted.output_size = "1920x1080".to_string();
        let err = validate(&wanted).unwrap_err();
        assert!(
            err.message.contains("cannot scale"),
            "the refusal names the reason: {}",
            err.message
        );

        // With the ffmpeg child path it is fine: ffmpeg scales.
        wanted.zero_copy = Some(false);
        assert!(validate(&wanted).is_ok());
    }

    /// The file's values are the panel's, and the example's fill in what the file omits.
    #[test]
    fn the_reader_falls_back_to_the_example_and_reports_the_file_where_it_can() {
        let (_dir, path) = config_with(
            "[encode]\nvendor = \"amf\"\ncodec = \"hevc\"\nbitrate_kbps = 8000\n\
             rate_control = \"vbr\"\npreset = \"balanced\"\n",
        );
        let read = encoder_settings(&path).unwrap();
        assert_eq!(read.vendor, "amf");
        assert_eq!(read.codec, "hevc");
        assert_eq!(read.bitrate_kbps, 8_000);
        assert_eq!(read.rate_control, "vbr");
        assert_eq!(read.preset, "balanced");
        // Not in the file: the example's.
        assert_eq!(read.fps, 60, "the example's capture rate");
        assert_eq!(read.audio_bitrate_kbps, 192, "the example's audio rate");
        assert!(read.adapt_fps, "the example's adaptation");
        assert_eq!(read.zero_copy, None, "the example leaves the key out");
    }

    /// The status names the encoder that was chosen, and what the path can do.
    #[test]
    fn the_status_names_the_encoder_that_was_chosen() {
        let (_dir, path) = config_with("[encode]\nvendor = \"auto\"\nzero_copy = false\n");
        let status = encoder_status(&path, Some("h264_nvenc")).unwrap();
        assert_eq!(status.requested_vendor, "auto");
        assert_eq!(status.selected_vendor.as_deref(), Some("nvenc"));
        assert_eq!(status.selected_codec.as_deref(), Some("h264"));
        assert!(status.capabilities.supports_scaling);
        assert!(status.capabilities.presets.contains(&"p4".to_string()));
        assert!(status.message.contains("h264_nvenc"), "{}", status.message);

        // Before a recording has run, nothing has been chosen, and the panel is told so.
        let fresh = encoder_status(&path, None).unwrap();
        assert_eq!(fresh.selected_vendor, None);
        assert_eq!(fresh.selected_codec, None);
        assert!(
            fresh.message.contains("no recording has run yet"),
            "{}",
            fresh.message
        );
    }

    /// The zero-copy path is the one that cannot scale, and the status says so.
    #[test]
    fn the_status_reports_the_zero_copy_paths_limits() {
        let (_dir, path) = config_with("[encode]\nvendor = \"auto\"\nzero_copy = true\n");
        let status = encoder_status(&path, Some("hevc_nvenc")).unwrap();
        assert_eq!(status.selected_vendor.as_deref(), Some("nvenc"));
        assert_eq!(status.selected_codec.as_deref(), Some("hevc"));
        assert!(!status.capabilities.supports_scaling);
        assert!(status.capabilities.presets.is_empty());
        assert_eq!(status.capabilities.rate_control_modes, vec!["cbr", "vbr"]);
        assert!(
            status.message.contains("cannot scale"),
            "the message says what the path cannot do: {}",
            status.message
        );
    }
}
