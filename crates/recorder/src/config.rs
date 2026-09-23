//! The configuration surface of the recorder: the three `config.toml` sections that
//! describe a recording.
//!
//! These are the *recording* settings, so they live with the thing they configure rather
//! than with either front-end. The CLI keeps `[hotkeys]` and `[events]` (a driver's
//! business, and no business of the engine's) and the desktop shell keeps its own view of
//! `[storage]` for the review pane; both parse the sections below for the recorder.
//!
//! The field names, the types and the defaults are `config.example.toml`'s (spec §10):
//! paths are `String` because an empty one means "the default under the application data
//! directory", which is a rule the recorder applies when it resolves them
//! ([`crate::RecorderConfig`]).

use serde::Deserialize;

/// `[buffer]` — the ring's window, its segment length and its scratch budget.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BufferSection {
    /// Media time kept before a trigger.
    pub pre_seconds: u64,
    /// Media time kept after a trigger.
    pub post_seconds: u64,
    /// Length of one scratch segment. Also the keyframe interval, which is what makes a
    /// clip a lossless concatenation of whole segments (spec §6.3).
    pub segment_time: u64,
    /// Total bytes the scratch ring may occupy. Enforced on every scan, and a violation
    /// is fatal: a ring that overruns its cap is a disk that fills up (spec §8.1).
    pub scratch_cap_bytes: u64,
    /// Empty means `<app data dir>/scratch`.
    pub scratch_dir: String,
}

/// `[encode]` — what the encoder is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EncodeSection {
    /// `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`. Resolved by a smoke test (spec §5.2).
    pub vendor: String,
    /// `"h264"` or `"hevc"`.
    pub codec: String,
    pub bitrate_kbps: u32,
    /// The frame rate asked for. It is the *ceiling* on the rate the pipeline runs at: what
    /// the machine can actually sustain is measured at startup (see `adapt_fps`) and the
    /// lower of the two is used for both the pacer and the encoder child.
    pub fps: u32,
    /// Measure the sustainable encode rate at startup and record at the lower of that and
    /// `fps` (default `true`).
    ///
    /// This is a **pacing** decision, not the timeline's guarantee. It exists so the capture
    /// does not pay for frames the encoder will throw away: on Windows every captured frame
    /// costs a GPU-to-CPU readback (33 MB at 4K) whether or not it is ever encoded, so a rate
    /// the machine cannot hold is wasted work and a choppier picture — on the measured 4K box
    /// a configured 30fps sustained ~24fps with ~45% of frames dropped (issue #1).
    ///
    /// What makes the *media timeline* track real time is independent of throughput and of
    /// this setting: frames carry their arrival timestamps and the encoder no longer resamples
    /// them onto a declared grid (`localplay_encoder::ffmpeg`), so a `pre_seconds` window is
    /// that many real seconds on a machine of any speed. With `adapt_fps = false` the frames
    /// the machine cannot encode are still dropped — the picture holds them — but the clock
    /// stays honest, and the drop counter in the status line says how many.
    ///
    /// The probe costs about 1.5s of startup. Set it to `false` to skip that and declare `fps`
    /// exactly as configured. A measurement at or above `fps` changes nothing either way.
    #[serde(default = "adapt_fps_default")]
    pub adapt_fps: bool,
    /// `"1920x1080"`, or empty for the capture backend's native size (spec §10).
    pub output_size: String,
}

/// `encode.adapt_fps` when the file does not mention it: on, because not paying a readback
/// for frames the encoder will drop is the better default, and a rate the machine cannot hold
/// is worth warning about before recording rather than after. A `config.toml` written before
/// this key existed keeps working and gets the measured behaviour.
fn adapt_fps_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// The `[encode]` section alone, so a test does not have to carry the whole file.
    #[derive(Debug, Deserialize)]
    struct File {
        encode: EncodeSection,
    }

    fn encode_section(text: &str) -> EncodeSection {
        toml::from_str::<File>(text).expect("the section parses").encode
    }

    const MINIMAL: &str = "\
[encode]
vendor = \"auto\"
codec = \"h264\"
bitrate_kbps = 20000
fps = 30
output_size = \"\"
";

    /// The shipped example enables adaptation, and it is the file a fresh install runs.
    #[test]
    fn the_example_config_asks_for_adaptation() {
        let section = encode_section(include_str!("../../../config.example.toml"));
        assert!(section.adapt_fps, "config.example.toml must enable adaptation");
    }

    /// A `config.toml` written before this key existed gets adaptation, not silence: the
    /// default is the point of the key, and the alternative is a user who upgrades and keeps
    /// the behaviour the ledger measured as broken.
    #[test]
    fn a_config_that_does_not_mention_the_key_enables_adaptation() {
        assert!(encode_section(MINIMAL).adapt_fps);
    }

    /// The opt-out is honoured, and it is the only way to declare a rate the machine may not
    /// hold (the escape hatch documented in `config.example.toml` and the spec).
    #[test]
    fn the_key_can_turn_adaptation_off() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = false");
        assert!(!encode_section(&text).adapt_fps);
    }

    /// A typo is a parse error rather than a silent default: `adapt_fps = "yes"` must not
    /// leave a user believing they turned something on or off.
    #[test]
    fn a_non_boolean_value_is_a_parse_error() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = \"yes\"");
        let err = toml::from_str::<File>(&text).expect_err("only true/false are accepted");
        assert!(
            err.to_string().contains("adapt_fps"),
            "the error names the key that is wrong: {err}"
        );
    }
}

/// `[storage]` — the clips directory and the policy applied to it (spec §8.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StorageSection {
    /// Empty means `<app data dir>/clips`.
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included. Nothing is
    /// deleted to satisfy a cap the favourites alone exceed.
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted.
    pub max_age_days: u64,
}
