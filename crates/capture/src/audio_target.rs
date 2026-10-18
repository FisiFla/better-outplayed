//! Which audio the game track records: the game process alone, or everything the speakers play.
//!
//! Per-application capture (WASAPI process loopback) keeps Discord, music and notifications out
//! of the clip. It needs a running game to target, so with none found, or where the OS cannot do
//! it, the whole default output is captured instead, and the log says which one is in use.

use crate::audio_recovery::SourceOpener;
use crate::audio_timeline::PacketSource;
use anyhow::Result;
use serde::Deserialize;

/// `[capture] game_audio`: what the game-audio track records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameAudioSource {
    /// Only the running watched game and its child processes; the whole output when none is running.
    #[default]
    App,
    /// Everything the default output device plays.
    System,
}

/// Finds the process id of the game to capture, or `None` when no watched game is running.
pub type GameFinder = Box<dyn FnMut() -> Option<u32> + Send>;

/// Opens the two kinds of loopback stream; the WASAPI implementation, or a fake in tests.
pub(crate) trait LoopbackFactory {
    type Stream: PacketSource;
    fn open_process(&mut self, pid: u32) -> Result<Self::Stream>;
    /// The default output, with its key (see [`SourceOpener::wanted`]).
    fn open_system(&mut self) -> Result<(Self::Stream, String)>;
    fn system_key(&mut self) -> Result<String>;
}

/// Opens the game's own audio when there is a game to target, and the whole output otherwise.
pub(crate) struct GameAudioOpener<F> {
    factory: F,
    find_game: Option<GameFinder>,
    /// A process whose activation failed, so it is not retried every check while it runs.
    refused_pid: Option<u32>,
}

impl<F: LoopbackFactory> GameAudioOpener<F> {
    /// `find_game` is `None` for whole-output capture.
    pub fn new(factory: F, find_game: Option<GameFinder>) -> Self {
        Self {
            factory,
            find_game,
            refused_pid: None,
        }
    }

    fn target(&mut self) -> Option<u32> {
        let pid = (self.find_game.as_mut()?)()?;
        (Some(pid) != self.refused_pid).then_some(pid)
    }
}

fn process_key(pid: u32) -> String {
    format!("process:{pid}")
}

impl<F: LoopbackFactory> SourceOpener for GameAudioOpener<F> {
    type Stream = F::Stream;

    fn open(&mut self) -> Result<(F::Stream, String)> {
        if let Some(pid) = self.target() {
            match self.factory.open_process(pid) {
                Ok(stream) => {
                    tracing::info!(
                        pid,
                        "game audio: capturing the game process only (process loopback)"
                    );
                    return Ok((stream, process_key(pid)));
                }
                Err(e) => {
                    tracing::warn!(pid, error = %format!("{e:#}"), "game audio: per-application capture failed; capturing the whole output instead");
                    self.refused_pid = Some(pid);
                }
            }
        }
        let (stream, key) = self.factory.open_system()?;
        tracing::info!(
            per_app = self.find_game.is_some(),
            "game audio: capturing everything the default output plays (endpoint loopback)"
        );
        Ok((stream, format!("system:{key}")))
    }

    fn wanted(&mut self) -> Result<String> {
        match self.target() {
            Some(pid) => Ok(process_key(pid)),
            None => Ok(format!("system:{}", self.factory.system_key()?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_recovery::RecoveringSource;
    use crate::audio_timeline::{AudioTimeline, PacketInfo};
    use crate::AudioFormat;
    use anyhow::anyhow;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A stream that says which source it is through the value of every sample.
    struct Tagged(u8, bool);

    impl PacketSource for Tagged {
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            if std::mem::take(&mut self.1) {
                out.extend(std::iter::repeat_n(self.0, 480 * 4));
                return Ok(Some(PacketInfo {
                    frames: 480,
                    time: None,
                    discontinuity: false,
                }));
            }
            Ok(None)
        }
    }

    #[derive(Default)]
    struct Fake {
        refuse_process: bool,
        opened: Vec<String>,
    }

    impl LoopbackFactory for Fake {
        type Stream = Tagged;
        fn open_process(&mut self, pid: u32) -> Result<Tagged> {
            if self.refuse_process {
                return Err(anyhow!("activation refused"));
            }
            self.opened.push(process_key(pid));
            Ok(Tagged(2, true))
        }
        fn open_system(&mut self) -> Result<(Tagged, String)> {
            self.opened.push("system".into());
            Ok((Tagged(1, true), "speakers".into()))
        }
        fn system_key(&mut self) -> Result<String> {
            Ok("speakers".into())
        }
    }

    /// A game finder whose answer the test changes.
    fn finder() -> (Arc<Mutex<Option<u32>>>, GameFinder) {
        let pid = Arc::new(Mutex::new(None));
        let read = pid.clone();
        (pid, Box::new(move || *read.lock().unwrap()))
    }

    #[test]
    fn the_game_is_captured_alone_once_it_runs_and_the_output_again_after_it_exits() {
        let (pid, find) = finder();
        let opener = GameAudioOpener::new(Fake::default(), Some(find));
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let ms = Duration::from_millis;
        let mut source =
            RecoveringSource::start(opener, AudioTimeline::new(format, ms(0)), ms(0)).unwrap();

        *pid.lock().unwrap() = Some(42);
        source.poll(ms(1_000)).unwrap();
        *pid.lock().unwrap() = None;
        source.poll(ms(2_000)).unwrap();
        source.poll(ms(2_010)).unwrap();

        let opened = &source_factory(&source).opened;
        assert_eq!(opened, &["system", "process:42", "system"]);
        assert_eq!(source.generation(), 3);
    }

    fn source_factory(source: &RecoveringSource<GameAudioOpener<Fake>>) -> &Fake {
        &source.opener().factory
    }

    #[test]
    fn a_refused_activation_falls_back_to_the_output_and_is_not_retried() {
        let (pid, find) = finder();
        *pid.lock().unwrap() = Some(7);
        let mut opener = GameAudioOpener::new(
            Fake {
                refuse_process: true,
                ..Fake::default()
            },
            Some(find),
        );
        let (_, key) = opener.open().unwrap();
        assert_eq!(key, "system:speakers");
        assert_eq!(
            opener.wanted().unwrap(),
            key,
            "so the open stream is kept, not reopened every second"
        );

        // A new game process is tried again.
        *pid.lock().unwrap() = Some(8);
        assert_eq!(opener.wanted().unwrap(), "process:8");
    }

    #[test]
    fn whole_output_mode_never_looks_for_the_game() {
        let mut opener = GameAudioOpener::new(Fake::default(), None);
        assert_eq!(opener.open().unwrap().1, "system:speakers");
        assert_eq!(opener.wanted().unwrap(), "system:speakers");
    }

    #[test]
    fn the_setting_reads_as_lowercase_words() {
        #[derive(Deserialize)]
        struct Section {
            game_audio: GameAudioSource,
        }
        let parse = |v: &str| {
            serde_json::from_str::<Section>(&format!("{{\"game_audio\":\"{v}\"}}"))
                .map(|s| s.game_audio)
        };
        assert_eq!(parse("app").unwrap(), GameAudioSource::App);
        assert_eq!(parse("system").unwrap(), GameAudioSource::System);
        assert!(parse("discord").is_err());
    }
}
