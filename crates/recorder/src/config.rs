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

/// `[recorder]` — which recording this run is (spec §6, Phase 5).
///
/// Its own section rather than a key on `[buffer]` or `[storage]`: the mode is not a
/// property of the ring or of the storage policy, it is what the engine is asked to do,
/// and the two modes read every other setting differently (the scratch cap applies to one
/// and not the other, and only one of them produces a session file).
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
/// * [`RecordingMode::ReplayBuffer`] — a bounded ring holding the last few minutes of play, a
///   hotkey (or a game event) splices a clip out of it, and everything older than
///   `buffer.ram_cap_bytes` is dropped. Nothing else survives the run, and — because the ring is
///   memory — nothing was written until a clip was asked for (§12 of the verification ledger).
/// * [`RecordingMode::FullSession`] — the whole session is written to disk: segments go
///   into a per-session directory under the sessions area, **the scratch cap is not
///   applied to them** (a full session is not a ring: evicting the oldest segment of a
///   four-hour recording would delete footage the user asked for), and stopping the
///   recording losslessly concatenates them into one `session-<timestamp>.mp4`.
///
/// The strings are the ones `sessions.mode` stores ([`RecordingMode::store_mode`]), and
/// a test pins the two spellings together so a rename cannot silently split them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum RecordingMode {
    /// The rolling replay buffer. The default, and the only mode before Phase 5.
    #[default]
    #[serde(rename = "buffer", alias = "replay_buffer", alias = "replaybuffer")]
    ReplayBuffer,
    /// Record the whole session and concatenate it at stop.
    #[serde(rename = "session", alias = "full_session", alias = "fullsession")]
    FullSession,
}

impl RecordingMode {
    /// The `config.toml` spelling (`[recorder] mode`), which is also the string
    /// [`RecordingMode::store_mode`] maps onto.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => "buffer",
            RecordingMode::FullSession => "session",
        }
    }

    /// The `sessions.mode` value this mode opens its row with (spec §5.5). The store owns
    /// the vocabulary; this is the mapping, in one place.
    pub fn store_mode(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => localplay_store::SESSION_MODE_BUFFER,
            RecordingMode::FullSession => localplay_store::SESSION_MODE_SESSION,
        }
    }

    /// Whether this mode writes a session file and keeps its segments free of the scratch
    /// cap. Named rather than compared, so the intent is readable at the call sites.
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

/// `[mic]` — the optional microphone track (spec §5.1, Phase 5).
///
/// **Off by default, and that is the whole posture of the feature.** A microphone is not
/// part of the game's own audio, a machine may have no capture endpoint at all, and a
/// recording that silently gained a track nobody asked for is a surprise with a legal
/// flavour on a voice-chat-enabled box. The user opts in by writing `enabled = true`.
///
/// When it *is* on, the engine sets `mic_audio` on the encode configuration (so ffmpeg
/// gets a second audio input and the output carries two audio tracks) and feeds that input
/// from the microphone backend. When it cannot — no capture endpoint, or a platform with
/// no microphone backend — the recording **fails at start** rather than producing video
/// with a silent microphone track (see `Recorder::start`).
///
/// Device choice is deliberately not configurable: the Windows backend opens the default
/// **communications** capture endpoint, which is the device the user actually talks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct MicSection {
    /// Record a microphone track alongside the game audio. Default `false`.
    pub enabled: bool,
}

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
    /// Empty means `<app data dir>/scratch`.
    pub scratch_dir: String,
    /// Bytes of **RAM** the in-memory replay ring may occupy. Default 256 MiB.
    ///
    /// This is the cap that matters for a replay buffer: unclipped footage lives in RAM rather
    /// than on the SSD (see `localplay_replay::MemoryRingBuffer`), so this number is what keeps
    /// the application from being killed by the operating system. Sized as a fraction of a
    /// typical machine rather than all of it — the encoder, the capture backends and the
    /// application all need room, and a ring that has evicted something is still a ring.
    ///
    /// Defaulted rather than required, so a `config.toml` written before this key existed still
    /// parses: a config file belongs to its user, and a key this build added is not their error
    /// (the same reasoning as `[mic]` and `[games]`).
    #[serde(default = "default_ram_cap_bytes")]
    pub ram_cap_bytes: u64,
}

/// 256 MiB. See [`BufferSection::ram_cap_bytes`] for why it is not larger.
fn default_ram_cap_bytes() -> u64 {
    256 * 1024 * 1024
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
    /// Hand captured frames to the hardware encoder **as GPU textures**, so their pixels never
    /// reach this process's memory (default `false`).
    ///
    /// What it removes, measured on the 4K box: the capture backend copies 33.2 MB out of VRAM per
    /// frame for ffmpeg to read back in, and that copy is ~97% of what the pipeline does (`capture=`
    /// on the periodic line). With this on, a hardware encoder MFT takes the texture directly and
    /// ffmpeg is told to copy the H.264 that comes out — a few hundred kilobytes a second across the
    /// pipe instead of gigabytes.
    ///
    /// Off by default because it is a **behavioural** change to the shipping encode path, and the
    /// verification it deserves is a soak on real hardware rather than a test suite: the pieces are
    /// each proven (the MFT takes 4K ARGB32 from this device, the async loop carries frames, ffmpeg
    /// reads the bitstream and produces the ring's fragments with a keyframe a second), but no
    /// recording has been made end to end with it yet.
    ///
    /// It needs a hardware encoder — a software one has no MFT to hand a texture to — and the
    /// texture handover is Media Foundation's, so this is Windows-only. Both are refused at startup
    /// with the reason rather than silently ignored.
    #[serde(default)]
    pub zero_copy: bool,
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

    /// A `[buffer]` section written before `ram_cap_bytes` existed must still parse.
    ///
    /// The key is `#[serde(default)]` for exactly this reason — a config file belongs to its user,
    /// and a key a later build added is not their error — and a default no test ever takes is a
    /// claim rather than a behaviour. Every other key in the section is required, so `ram_cap_bytes`
    /// is the one this section cannot be missing.
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

    /// The mode is a setting of the engine, so it parses from `[recorder]` — and absent, it
    /// is the replay buffer, i.e. the behaviour every existing configuration already has.
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

    /// A mode this build does not know is a parse error naming the key — the alternative is
    /// recording the *other* mode while the file says otherwise.
    #[test]
    fn an_unknown_mode_is_refused() {
        let err = toml::from_str::<RecorderFile>("[recorder]\nmode = \"everything\"\n")
            .expect_err("only the two known modes are accepted");
        assert!(err.to_string().contains("mode"), "the error names the key: {err}");
    }

    /// The spelling `config.toml` uses and the value `sessions.mode` stores are one fact:
    /// the config's `"session"` must be the store's `SESSION_MODE_SESSION`, or a row would
    /// say one thing and the config another.
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

    /// The session rules are additive: a `[storage]` table written before Phase 5 parses,
    /// keeps its own values, and gets the documented default for the session store.
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

/// `[storage]` — the clips directory and the policy applied to it (spec §8.1).
///
/// Phase 5 added [`StorageSection::sessions`], additively: a file written before it existed
/// still parses, and every field above means exactly what it always did.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StorageSection {
    /// Empty means `<app data dir>/clips`.
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included. Nothing is
    /// deleted to satisfy a cap the favourites alone exceed.
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted.
    pub max_age_days: u64,
    /// `[storage.sessions]` — the same two rules, for the session store (Phase 5).
    /// Optional: a configuration that does not mention it gets
    /// [`SessionStorageRules::default`].
    #[serde(default)]
    pub sessions: SessionStorageRules,
}

/// `[storage.sessions]` — the session store's own rules (spec §8.1).
///
/// The field names are the clips rules' field names on purpose: sessions and clips are
/// evicted independently, by the *same* two rules with their own numbers, and a reader who
/// knows one table knows the other ([`crate::index::cleanup_pass`] applies both).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SessionStorageRules {
    /// Empty means `<app data dir>/sessions`.
    pub sessions_dir: String,
    /// Total bytes the session store may occupy, favourites and *running* sessions
    /// included. A session that is still recording is never an eviction candidate — its
    /// bytes are counted, and a live recording that alone exceeds the cap is reported as
    /// an unsatisfiable cap rather than paid for out of the finished sessions beside it.
    pub max_total_bytes: u64,
    /// A non-favourited session that *started* longer ago than this many days is deleted.
    pub max_age_days: u64,
}

impl Default for SessionStorageRules {
    /// 200 GiB and a week. The cap is generous because a session is one file the user
    /// deliberately recorded end to end (a 4K session is ~9 GB an hour at the example
    /// bitrate), and the age limit is the clips limit: a session is not more precious than
    /// a clip. Both are meant to be raised by a user who records a lot, not to be the
    /// thing that decides how much they can keep.
    fn default() -> Self {
        Self { sessions_dir: String::new(), max_total_bytes: 214_748_364_800, max_age_days: 7 }
    }
}
