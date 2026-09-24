//! Root configuration: TOML in the app data directory, never environment variables.
//!
//! The `[buffer]`, `[encode]` and `[storage]` sections are the recorder's own types
//! (`localplay_recorder::config`), because they describe the recording and the desktop
//! shell parses them too. What stays here is what a headless front-end owns: the hotkey
//! and the (Phase 4) game-event sources.

use anyhow::{bail, Context, Result};
use localplay_events::process::GamesSection;
use localplay_recorder::config::{
    BufferSection, EncodeSection, MicSection, RecorderSection, StorageSection,
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub recorder: RecorderSection,
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub audio: AudioSection,
    pub storage: StorageSection,
    #[serde(default)]
    pub mic: MicSection,
    pub hotkeys: HotkeySection,
    pub events: EventsSection,
    /// `[games]` — the game watcher (Phase 5, spec §7). Defaulted, so a file that does not
    /// mention it means "watch nothing": `auto_record` is `false` in `GamesSection::default`,
    /// and with it off the recorder starts no watcher at all.
    #[serde(default)]
    pub games: GamesSection,
}

#[derive(Debug, Deserialize)]
pub struct AudioSection {
    pub enabled: bool,
    pub source: String,
    pub codec: String,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Deserialize)]
pub struct HotkeySection {
    pub clip: String,
}

/// `[events]` — which game-event sources to run (spec §7.1, §7.2).
///
/// Two settings, and no more: the endpoint the League poller uses is fixed by the spec, the
/// GSI path is fixed, and the token is generated into the application data directory rather
/// than written in a file a user edits (see `gsi::load_or_create_token`).
#[derive(Debug, Deserialize)]
pub struct EventsSection {
    /// Poll the League Live Client Data API (spec §7.1). Harmless when no game is running:
    /// a refused loopback connection is the normal state and is not an error.
    pub lol_poll_enabled: bool,
    /// The port the CS2 / Dota 2 GSI listener binds on loopback, or `0` to leave it off.
    pub gsi_port: u16,
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text)
    }

    fn validate(&self) -> Result<()> {
        // Fail closed: there is no CPU encoder to fall back to (spec §3.2).
        match self.encode.vendor.as_str() {
            "auto" | "nvenc" | "qsv" | "amf" => {}
            "software" => bail!(
                "vendor = \"software\" is not supported: localplay requires a GPU hardware \
                 encoder (nvenc, qsv or amf) so that capturing does not cost game performance"
            ),
            other => bail!("unknown encode.vendor: {other}"),
        }
        // 0 disables the listener (config.example.toml documents it): a user who does not
        // want it should not have to pick a port they will never use. Anything else is a
        // real port, and the default sits above the ephemeral range so that a temporarily
        // bound listener elsewhere cannot shadow it at startup.
        if self.events.gsi_port != 0 && !(1024..=65535).contains(&self.events.gsi_port) {
            bail!(
                "events.gsi_port must be 0 (disabled) or between 1024 and 65535, got {}",
                self.events.gsi_port
            );
        }
        if self.buffer.segment_time == 0 {
            bail!("buffer.segment_time must be at least 1 second");
        }
        if self.buffer.post_seconds == 0 {
            bail!("buffer.post_seconds must be at least 1 second");
        }
        // The rate the pipeline declares to the encoder (its `-framerate`) and paces capture
        // to. Zero is not a rate: it makes `1 / fps` meaningless and ffmpeg's `-framerate 0`
        // an error, and the engine would clamp it to 1 rather than guess. Say so instead.
        if self.encode.fps == 0 {
            bail!(
                "encode.fps must be at least 1: it is the ceiling on the frame rate the \
                 pipeline paces to (it measures itself against this — see encode.adapt_fps)"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_example_config() {
        let text = include_str!("../../../config.example.toml");
        let cfg = Config::from_toml(text).expect("config.example.toml must stay valid");
        assert_eq!(cfg.buffer.pre_seconds, 30);
        assert_eq!(cfg.buffer.post_seconds, 5);
        assert!(cfg.audio.enabled);
        assert_eq!(cfg.hotkeys.clip, "Ctrl+F8");
        // The Phase 5 sections, in the file a fresh install runs: the replay buffer (the
        // mode every existing configuration already had), no microphone, and nothing
        // watched — which is the safety posture, not a placeholder.
        assert_eq!(cfg.recorder.mode, localplay_recorder::RecordingMode::ReplayBuffer);
        assert!(!cfg.mic.enabled, "the example must not turn the microphone on");
        assert!(!cfg.games.auto_record, "the example must not auto-record");
        assert!(cfg.storage.sessions.max_total_bytes > 0, "the session rules have defaults");
    }

    /// A `config.toml` written before Phase 5 parses — the new sections are all optional —
    /// and means exactly what it did: buffer mode, no microphone, nothing watched.
    #[test]
    fn a_pre_phase_5_config_still_parses_and_means_what_it_did() {
        // The pre-Phase-5 surface, verbatim: the three recording sections, the CLI's own
        // `[hotkeys]` and `[events]`, and nothing else.
        let text = "\
[buffer]
pre_seconds = 30
post_seconds = 5
segment_time = 1
scratch_cap_bytes = 2147483648
scratch_dir = \"\"

[encode]
vendor = \"auto\"
codec = \"h264\"
bitrate_kbps = 20000
fps = 60
adapt_fps = true
output_size = \"\"

[audio]
enabled = true
source = \"loopback\"
codec = \"aac\"
bitrate_kbps = 192

[storage]
clips_dir = \"\"
max_total_bytes = 53687091200
max_age_days = 7

[hotkeys]
clip = \"Ctrl+F8\"

[events]
lol_poll_enabled = true
gsi_port = 45671
";
        let cfg = Config::from_toml(text).expect("a pre-Phase-5 file must keep parsing");
        assert_eq!(cfg.recorder.mode, localplay_recorder::RecordingMode::ReplayBuffer);
        assert!(!cfg.mic.enabled, "no [mic] means no microphone");
        assert!(!cfg.games.auto_record, "no [games] means nothing is watched");
        assert_eq!(cfg.games.watch, localplay_events::process::default_watch(), "with the default watch list, unused while auto_record is off");
        assert_eq!(
            cfg.storage.sessions,
            localplay_recorder::config::SessionStorageRules::default(),
            "and the session rules are the documented default"
        );
    }

    /// The mode is settable from the file — that is what makes the CLI's `--mode` an
    /// *override* rather than the only way in.
    #[test]
    fn the_mode_can_be_set_from_the_file() {
        let text = include_str!("../../../config.example.toml")
            .replace("mode = \"buffer\"", "mode = \"session\"");
        let cfg = Config::from_toml(&text).expect("the session mode parses");
        assert_eq!(cfg.recorder.mode, localplay_recorder::RecordingMode::FullSession);
    }

    /// The microphone is opt-in, and `[mic] enabled = true` is the whole opt-in.
    #[test]
    fn the_microphone_is_opt_in_through_the_file() {
        let text = include_str!("../../../config.example.toml")
            .replace("[mic]\nenabled = false", "[mic]\nenabled = true");
        assert!(text.contains("[mic]\nenabled = true"), "the substitution must apply");
        let cfg = Config::from_toml(&text).expect("mic.enabled = true parses");
        assert!(cfg.mic.enabled);
    }

    #[test]
    fn rejects_a_software_encoder_request() {
        let text = include_str!("../../../config.example.toml")
            .replace("vendor = \"auto\"", "vendor = \"software\"");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("software"),
            "must explain that CPU encoding is not available: {err}"
        );
    }

    #[test]
    fn rejects_a_gsi_port_above_the_ephemeral_range_start() {
        let text = include_str!("../../../config.example.toml")
            .replace("gsi_port = 45671", "gsi_port = 80");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("gsi_port"), "got: {err}");
    }

    #[test]
    fn a_zero_gsi_port_means_the_listener_is_off() {
        // The documented way to disable an integration that would otherwise want a port.
        let text = include_str!("../../../config.example.toml")
            .replace("gsi_port = 45671", "gsi_port = 0");
        let cfg = Config::from_toml(&text).expect("0 is a valid setting");
        assert_eq!(cfg.events.gsi_port, 0);
    }

    /// The frame rate is what the pipeline declares and paces to, so zero has to fail with a
    /// sentence rather than be silently clamped to 1 by the engine.
    #[test]
    fn rejects_a_zero_frame_rate() {
        let text = include_str!("../../../config.example.toml").replace("fps = 60", "fps = 0");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("encode.fps"), "got: {err}");
    }

    /// The example enables adaptation, so a fresh install measures what the machine can hold
    /// (the behaviour issues #1 and #2 need); a file without the key gets it too, and the
    /// opt-out is honoured. Parsed through the CLI's own reader, which is what runs.
    #[test]
    fn the_example_enables_adaptation_and_the_key_can_turn_it_off() {
        let example = Config::from_toml(include_str!("../../../config.example.toml"))
            .expect("the example is valid");
        assert!(example.encode.adapt_fps, "config.example.toml must enable adaptation");

        let text = include_str!("../../../config.example.toml")
            .replace("adapt_fps = true", "adapt_fps = false");
        let off = Config::from_toml(&text).expect("false is a valid setting");
        assert!(!off.encode.adapt_fps);

        // And a file that never mentions the key still adapts: the default lives in the
        // shared section type, which this reader deserialises.
        let text = include_str!("../../../config.example.toml")
            .lines()
            .filter(|l| !l.trim_start().starts_with("adapt_fps"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("fps = 60")
                && !text.lines().any(|l| l.trim_start().starts_with("adapt_fps =")),
            "the setting must be gone from the text being parsed (a comment mentioning the \
             key is fine — the point is that no value is set)"
        );
        let default = Config::from_toml(&text).expect("the key is optional");
        assert!(default.encode.adapt_fps, "the default is measured behaviour, not silence");
    }

    #[test]
    fn the_capture_sections_are_the_recorders_own_types() {
        // The CLI does not re-declare `[buffer]`, `[encode]` or `[storage]`: the desktop
        // shell parses the same sections into the same types, which is what keeps one
        // config file meaning one thing to both front-ends.
        let cfg = Config::from_toml(include_str!("../../../config.example.toml")).unwrap();
        let buffer: localplay_recorder::BufferSection = cfg.buffer;
        let encode: localplay_recorder::EncodeSection = cfg.encode;
        let storage: localplay_recorder::StorageSection = cfg.storage;
        assert_eq!(buffer.segment_time, 1);
        assert_eq!(encode.codec, "h264");
        assert_eq!(storage.max_age_days, 7);
        assert!(storage.sessions.max_age_days > 0, "the sessions rules are part of the same section");
    }
}
