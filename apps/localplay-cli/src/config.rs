//! Root configuration: TOML in the app data directory, never environment variables.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub audio: AudioSection,
    pub storage: StorageSection,
    pub hotkeys: HotkeySection,
    pub events: EventsSection,
}

#[derive(Debug, Deserialize)]
pub struct BufferSection {
    pub pre_seconds: u64,
    pub post_seconds: u64,
    pub segment_time: u64,
    pub scratch_cap_bytes: u64,
    pub scratch_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct EncodeSection {
    pub vendor: String,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub output_size: String,
}

#[derive(Debug, Deserialize)]
pub struct AudioSection {
    pub enabled: bool,
    pub source: String,
    pub codec: String,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Deserialize)]
pub struct StorageSection {
    pub clips_dir: String,
    pub max_total_bytes: u64,
    pub max_age_days: u64,
}

#[derive(Debug, Deserialize)]
pub struct HotkeySection {
    pub clip: String,
}

#[derive(Debug, Deserialize)]
pub struct EventsSection {
    pub lol_poll_enabled: bool,
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
        if !(1024..=65535).contains(&self.events.gsi_port) {
            bail!(
                "events.gsi_port must be between 1024 and 65535, got {}",
                self.events.gsi_port
            );
        }
        if self.buffer.segment_time == 0 {
            bail!("buffer.segment_time must be at least 1 second");
        }
        if self.buffer.post_seconds == 0 {
            bail!("buffer.post_seconds must be at least 1 second");
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
}
