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
    pub fps: u32,
    /// `"1920x1080"`, or empty for the capture backend's native size (spec §10).
    pub output_size: String,
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
