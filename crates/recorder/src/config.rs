//! The recorder's `config.toml` sections: `[recorder]`, `[mic]`, `[buffer]`, `[capture]`,
//! `[encode]`, `[storage]`.
//!
//! They live with the engine they configure; the CLI keeps `[hotkeys]` and `[events]` and the
//! desktop shell its own `[storage]` view. Names, types and defaults follow `config.example.toml`.
//! Paths are `String` because an empty one means "the default under the application data
//! directory", which [`crate::RecorderConfig`] applies when resolving them.

use serde::Deserialize;

/// `[recorder]`: which recording this run is.
///
/// Its own section because the mode is what the engine is asked to do, not a property of the ring
/// or the storage policy: the scratch cap applies to one mode only, and only one produces a session file.
///
/// ```toml
/// [recorder]
/// mode = "buffer"   # or "session"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct RecorderSection {
    /// [`RecordingMode::ReplayBuffer`] (the default) or [`RecordingMode::FullSession`].
    pub mode: RecordingMode,
}

/// What the engine records.
///
/// * [`RecordingMode::ReplayBuffer`]: a bounded RAM ring of the last few minutes; a hotkey or game
///   event splices a clip out of it, older footage is dropped at `buffer.ram_cap_bytes`, and
///   nothing is written until a clip is asked for.
/// * [`RecordingMode::FullSession`]: segments go into a per-session directory that the scratch cap
///   does not apply to (evicting the oldest segment of a long recording would delete footage the
///   user asked for); stopping losslessly concatenates them into one `session-<timestamp>.mp4`.
///
/// The strings are those `sessions.mode` stores ([`RecordingMode::store_mode`]); a test pins the
/// spellings together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum RecordingMode {
    /// The rolling replay buffer (the default).
    #[default]
    #[serde(rename = "buffer", alias = "replay_buffer", alias = "replaybuffer")]
    ReplayBuffer,
    /// Record the whole session and concatenate it at stop.
    #[serde(rename = "session", alias = "full_session", alias = "fullsession")]
    FullSession,
}

impl RecordingMode {
    /// The `config.toml` spelling (`[recorder] mode`), also the string [`RecordingMode::store_mode`] maps onto.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => "buffer",
            RecordingMode::FullSession => "session",
        }
    }

    /// The `sessions.mode` value this mode opens its row with; the store owns the vocabulary.
    pub fn store_mode(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => localplay_store::SESSION_MODE_BUFFER,
            RecordingMode::FullSession => localplay_store::SESSION_MODE_SESSION,
        }
    }

    /// Whether this mode writes a session file and keeps its segments free of the scratch cap.
    pub fn is_full_session(self) -> bool {
        matches!(self, RecordingMode::FullSession)
    }
}

impl std::fmt::Display for RecordingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RecordingMode {
    type Err = anyhow::Error;

    /// The CLI's `--mode` parsing, and the one place the accepted spellings are listed.
    fn from_str(spec: &str) -> anyhow::Result<Self> {
        match spec.trim().to_ascii_lowercase().as_str() {
            "buffer" | "replay" | "replay_buffer" => Ok(RecordingMode::ReplayBuffer),
            "session" | "full" | "full_session" => Ok(RecordingMode::FullSession),
            other => anyhow::bail!(
                "unknown recording mode {other:?}: expected \"buffer\" (rolling replay \
                 buffer) or \"session\" (record the whole session)"
            ),
        }
    }
}

/// `[mic]`: the optional microphone track, off by default.
///
/// Opt-in because a microphone is not the game's audio, a machine may have no capture endpoint, and
/// a silently added voice track is a privacy surprise. When on, the engine sets `mic_audio` on the
/// encode config (a second audio input, two output tracks) and feeds it from the microphone backend.
/// If it cannot (no endpoint, or no backend on this platform) the recording fails at start rather
/// than carrying a silent track (see `Recorder::start`).
///
/// Which device is recorded is `device`: a WASAPI endpoint id from the settings picker, or empty
/// for the default communications capture endpoint (the device the user talks into). A chosen
/// device that is missing at start refuses the recording rather than recording another one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct MicSection {
    /// Record a microphone track alongside the game audio. Default `false`.
    pub enabled: bool,
    /// The capture endpoint id to record; empty is the default communications microphone.
    pub device: String,
}

/// `[buffer]`: the ring's window, segment length and budgets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BufferSection {
    /// Media time kept before a trigger.
    pub pre_seconds: u64,
    /// Media time kept after a trigger.
    pub post_seconds: u64,
    /// Length of one scratch segment, also the keyframe interval, so a clip is a lossless
    /// concatenation of whole segments.
    pub segment_time: u64,
    /// Empty means `<app data dir>/scratch`.
    pub scratch_dir: String,
    /// Bytes of RAM the in-memory replay ring may occupy. Default 256 MiB.
    ///
    /// Unclipped footage lives in RAM (see `localplay_replay::MemoryRingBuffer`), so this keeps the OS
    /// from killing the application. It is a fraction of a typical machine because the encoder and
    /// capture backends need room too. Defaulted so an older `config.toml` still parses.
    #[serde(default = "default_ram_cap_bytes")]
    pub ram_cap_bytes: u64,
    /// The longest pre-roll a single clip may ask for, in seconds; `0` means `pre_seconds`. It sizes
    /// the ring so a long-clip hotkey can reach further back. Larger requests are clamped, not errors.
    #[serde(default)]
    pub max_pre_seconds: u64,
    /// The longest post-roll a single clip may ask for, in seconds; `0` means `post_seconds`.
    #[serde(default)]
    pub max_post_seconds: u64,
}

impl BufferSection {
    /// The pre-roll ceiling in milliseconds: the larger of `pre_seconds` and `max_pre_seconds`.
    pub fn max_pre_ms(&self) -> u64 {
        self.pre_seconds.max(self.max_pre_seconds) * 1000
    }

    /// The post-roll ceiling in milliseconds: the larger of `post_seconds` and `max_post_seconds`.
    pub fn max_post_ms(&self) -> u64 {
        self.post_seconds.max(self.max_post_seconds) * 1000
    }
}

/// 256 MiB. See [`BufferSection::ram_cap_bytes`] for why it is not larger.
fn default_ram_cap_bytes() -> u64 {
    256 * 1024 * 1024
}

/// `[capture]`: what is recorded, a monitor or one window.
///
/// Both keys are optional and default to the primary monitor. Choosing is the capture crate's
/// ([`localplay_capture::target`]); only Windows Graphics Capture honours it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct CaptureSection {
    /// Monitor index; 0 is the primary. An index with no monitor refuses to start rather than record another display.
    pub monitor: usize,
    /// A window title (case-insensitive; exact beats substring) to capture instead of a monitor.
    /// Empty captures the monitor; no match refuses to start.
    pub window: String,
    /// `"app"`: only the running watched game (the whole output while none runs); `"system"`:
    /// everything the default output plays.
    pub game_audio: localplay_capture::GameAudioSource,
    /// The output endpoint id whose audio is recorded when the whole output is captured (`"system"`,
    /// or `"app"` with no game running). Empty is the Windows default output. A game's own process
    /// audio follows the game whatever device it plays on, so this does not apply to it.
    pub output_device: String,
}

impl CaptureSection {
    pub fn target(&self) -> localplay_capture::target::CaptureTarget {
        localplay_capture::target::CaptureTarget::from_config(self.monitor, &self.window)
    }
}

/// `[encode]` — what the encoder is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EncodeSection {
    /// `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`, resolved by a smoke test.
    pub vendor: String,
    /// `"h264"` or `"hevc"`.
    pub codec: String,
    pub bitrate_kbps: u32,
    /// The frame rate asked for: a ceiling. The sustainable rate is measured at startup (see
    /// `adapt_fps`) and the lower of the two drives both the pacer and the encoder child.
    pub fps: u32,
    /// Hand captured frames to the hardware encoder as GPU textures so pixels never reach this
    /// process's memory (default: unset).
    ///
    /// This removes the per-frame VRAM readback (33.2 MB at 4K), which dominated the pipeline: a
    /// hardware encoder MFT takes the texture directly and ffmpeg copies the H.264 that comes out.
    ///
    /// * Unset: use it when the hardware can (a detected hardware encoder on Windows), otherwise the
    ///   ordinary path with a log line. Refusing to start by default would break the recorder on any
    ///   other machine and the test suite.
    /// * `true`: the hardware path is required; a run that cannot have it is refused at startup with
    ///   the reason rather than silently downgraded. This is why it is an `Option`, not a `bool`.
    /// * `false`: off everywhere.
    ///
    /// Needs a hardware encoder (a software one has no MFT to take a texture) and is Windows-only.
    #[serde(default)]
    pub zero_copy: Option<bool>,
    /// Measure the sustainable encode rate at startup and record at the lower of that and `fps`
    /// (default `true`).
    ///
    /// A pacing decision, not the timeline guarantee: on Windows every captured frame costs a
    /// GPU-to-CPU readback (33 MB at 4K) whether or not it is encoded, so a rate the machine cannot
    /// hold is wasted work and a choppier picture. The media timeline tracks real time regardless,
    /// because frames carry their arrival timestamps (`localplay_encoder::ffmpeg`). With
    /// `adapt_fps = false` the unencodable frames are still dropped (the picture holds them) and the
    /// status line's drop counter says how many.
    ///
    /// The probe costs about 1.5s of startup; `false` skips it and declares `fps` exactly.
    #[serde(default = "adapt_fps_default")]
    pub adapt_fps: bool,
    /// `"1920x1080"`, or empty for the capture backend's native size.
    pub output_size: String,
}

/// `encode.adapt_fps` when the file does not mention it: on, since skipping readbacks for frames
/// the encoder would drop is the better default and an unholdable rate deserves a warning up front.
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

    /// The same for `[recorder]`, with the section optional so "absent" can be tested.
    #[derive(Debug, Deserialize)]
    struct RecorderFile {
        #[serde(default)]
        recorder: RecorderSection,
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

    /// A `[buffer]` section written before `ram_cap_bytes` existed must still parse; it is the one key
    /// the section can be missing, and an untaken default is a claim rather than a behaviour.
    #[test]
    fn a_config_without_the_ram_cap_parses_and_gets_the_default() {
        #[derive(Debug, Deserialize)]
        struct File {
            buffer: BufferSection,
        }
        let text = format!(
            "{MINIMAL}\n[buffer]\npre_seconds = 30\npost_seconds = 5\nsegment_time = 1\n\
             scratch_dir = \"\"\n"
        );
        let file = toml::from_str::<File>(&text).expect("a config from before this key parses");
        assert_eq!(
            file.buffer.ram_cap_bytes,
            256 * 1024 * 1024,
            "an absent key must default, not fail and not be zero"
        );
    }

    /// The opt-out is honoured; it is the only way to declare a rate the machine may not hold.
    #[test]
    fn the_key_can_turn_adaptation_off() {
        assert!(encode_section(MINIMAL).adapt_fps, "absent means on");
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = false");
        assert!(!encode_section(&text).adapt_fps);
    }

    /// A typo is a parse error rather than a silent default: `adapt_fps = "yes"` must not leave a user misled.
    #[test]
    fn a_non_boolean_value_is_a_parse_error() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = \"yes\"");
        let err = toml::from_str::<File>(&text).expect_err("only true/false are accepted");
        assert!(
            err.to_string().contains("adapt_fps"),
            "the error names the key that is wrong: {err}"
        );
    }

    /// The mode parses from `[recorder]`; absent, it is the replay buffer.
    #[test]
    fn the_recording_mode_parses_and_defaults_to_the_replay_buffer() {
        let absent: RecorderFile = toml::from_str("buffer_thing = 1\n").expect("no [recorder]");
        assert_eq!(absent.recorder.mode, RecordingMode::ReplayBuffer);

        let section: RecorderFile =
            toml::from_str("[recorder]\nmode = \"buffer\"\n").expect("the buffer spelling");
        assert_eq!(section.recorder.mode, RecordingMode::ReplayBuffer);

        let session: RecorderFile =
            toml::from_str("[recorder]\nmode = \"session\"\n").expect("the session spelling");
        assert_eq!(session.recorder.mode, RecordingMode::FullSession);
        assert!(session.recorder.mode.is_full_session());
    }

    /// An unknown mode is a parse error naming the key, not a silent fallback to the other mode.
    #[test]
    fn an_unknown_mode_is_refused() {
        let err = toml::from_str::<RecorderFile>("[recorder]\nmode = \"everything\"\n")
            .expect_err("only the two known modes are accepted");
        assert!(err.to_string().contains("mode"), "the error names the key: {err}");
    }

    /// The config's `"session"` must be the store's `SESSION_MODE_SESSION`, or a row and the config would disagree.
    #[test]
    fn the_mode_spellings_match_the_stores_vocabulary() {
        for mode in [RecordingMode::ReplayBuffer, RecordingMode::FullSession] {
            assert_eq!(mode.as_str(), mode.store_mode(), "{mode:?}");
            assert_eq!(mode.to_string(), mode.store_mode());
            assert_eq!(mode.as_str().parse::<RecordingMode>().unwrap(), mode);
        }
        assert_eq!(RecordingMode::FullSession.store_mode(), localplay_store::SESSION_MODE_SESSION);
        assert_eq!(RecordingMode::ReplayBuffer.store_mode(), localplay_store::SESSION_MODE_BUFFER);
    }

    /// The microphone is opt-in, and the section's absence is the same answer as `false`.
    #[test]
    fn the_microphone_is_off_unless_the_file_turns_it_on() {
        #[derive(Debug, Deserialize)]
        struct File {
            #[serde(default)]
            mic: MicSection,
        }
        let absent: File = toml::from_str("nothing = 1\n").expect("no [mic]");
        assert!(!absent.mic.enabled, "the microphone is off by default");
        let empty: File = toml::from_str("[mic]\n").expect("[mic] with nothing in it");
        assert!(!empty.mic.enabled);
        let on: File = toml::from_str("[mic]\nenabled = true\n").expect("the opt-in");
        assert!(on.mic.enabled);
    }

    /// An older `[storage]` table still parses, keeps its values and gets the default session rules.
    #[test]
    fn the_session_storage_rules_are_optional_and_separate() {
        #[derive(Debug, Deserialize)]
        struct File {
            storage: StorageSection,
        }
        let old: File = toml::from_str(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1000\nmax_age_days = 3\n",
        )
        .expect("a pre-Phase-5 [storage] table must keep parsing");
        assert_eq!(old.storage.max_total_bytes, 1_000);
        assert_eq!(old.storage.max_age_days, 3);
        assert_eq!(old.storage.sessions, SessionStorageRules::default(), "the default rules");
        assert!(old.storage.sessions.max_total_bytes > old.storage.max_total_bytes);

        let extended: File = toml::from_str(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1000\nmax_age_days = 3\n\n\
             [storage.sessions]\nmax_total_bytes = 500\nmax_age_days = 1\n",
        )
        .expect("the session table");
        assert_eq!(extended.storage.sessions.max_total_bytes, 500);
        assert_eq!(extended.storage.sessions.max_age_days, 1);
        assert_eq!(extended.storage.sessions.sessions_dir, "", "empty means the default");
        assert_eq!(extended.storage.max_total_bytes, 1_000, "the clips rule is untouched");
    }
}

/// `[storage]`: the clips directory and the policy applied to it. [`StorageSection::sessions`] is additive,
/// so older files still parse.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StorageSection {
    /// Empty means `<app data dir>/clips`.
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included. Nothing is
    /// deleted to satisfy a cap the favourites alone exceed.
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted.
    pub max_age_days: u64,
    /// `[storage.sessions]`: the same two rules for the session store. Optional; absent gets
    /// [`SessionStorageRules::default`].
    #[serde(default)]
    pub sessions: SessionStorageRules,
}

/// `[storage.sessions]`: the session store's own rules.
///
/// Field names mirror the clips rules because sessions and clips are evicted independently by the
/// same two rules with their own numbers ([`crate::index::cleanup_pass`] applies both).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SessionStorageRules {
    /// Empty means `<app data dir>/sessions`.
    pub sessions_dir: String,
    /// Total bytes the session store may occupy, favourites and running sessions included. A running
    /// session is never an eviction candidate; a live recording that alone exceeds the cap is reported
    /// as an unsatisfiable cap rather than paid for out of finished sessions.
    pub max_total_bytes: u64,
    /// A non-favourited session that started longer ago than this many days is deleted.
    pub max_age_days: u64,
}

impl Default for SessionStorageRules {
    /// 200 GiB and a week. Generous because a session is one file the user recorded deliberately end
    /// to end; users who record a lot should raise it.
    fn default() -> Self {
        Self { sessions_dir: String::new(), max_total_bytes: 214_748_364_800, max_age_days: 7 }
    }
}
