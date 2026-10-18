//! The health page's data, and the GSI installer behind the settings button.
//!
//! Everything here is a plain function over explicit paths and values, so tests drive it
//! against temporary directories and hand-built statuses with no window or Tauri runtime.
//!
//! # The GSI installer
//!
//! The application does not write into game installation directories on its own. The installer
//! runs only when the user presses the button, writes exactly one file
//! (`gamestate_integration_localplay.cfg`), and never takes the destination from the webview:
//! the command receives a game (`cs2` or `dota2`), the install directory is found here from
//! Steam's own library list, and the final path goes through
//! [`crate::paths::ensure_new_file_within`] against that directory before a byte is written, so
//! a symlink or `..` cannot walk the write out of the game's folder.

use crate::commands::{CommandError, ErrorCode};
use localplay_events::gsi;
use localplay_recorder::event_driver::{SourceReport, SourceState};
use localplay_recorder::RecorderStatus;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file name both games read.
pub const GSI_CFG_NAME: &str = "gamestate_integration_localplay.cfg";

// ---------------------------------------------------------------------------------------
// Game directories
// ---------------------------------------------------------------------------------------

/// The two games with a Game State Integration feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GsiGame {
    Cs2,
    Dota2,
}

impl GsiGame {
    pub const ALL: [GsiGame; 2] = [GsiGame::Cs2, GsiGame::Dota2];

    pub fn label(self) -> &'static str {
        match self {
            GsiGame::Cs2 => "Counter-Strike 2",
            GsiGame::Dota2 => "Dota 2",
        }
    }

    /// The folder under `steamapps/common`.
    fn folder(self) -> &'static str {
        match self {
            GsiGame::Cs2 => "Counter-Strike Global Offensive",
            GsiGame::Dota2 => "dota 2 beta",
        }
    }

    /// Where the cfg goes, relative to the install folder.
    fn cfg_dir(self) -> &'static [&'static str] {
        match self {
            GsiGame::Cs2 => &["game", "csgo", "cfg"],
            GsiGame::Dota2 => &["game", "dota", "cfg", "gamestate_integration"],
        }
    }

    fn render(self, port: u16, token: &str) -> String {
        match self {
            GsiGame::Cs2 => gsi::integration_cfg(port, token),
            GsiGame::Dota2 => gsi::integration_cfg_dota(port, token),
        }
    }
}

/// Steam's own roots, from the environment. Whatever does not exist is filtered later.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(PathBuf::from(base).join("Steam"));
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        roots.push(home.join("Library/Application Support/Steam"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join(".local/share/Steam"));
    }
    roots
}

/// Every Steam library: the roots themselves plus the `"path"` entries of each root's
/// `steamapps/libraryfolders.vdf`.
pub fn steam_libraries(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if p.is_dir() && !libs.contains(&p) {
            libs.push(p);
        }
    };
    for root in roots {
        add(root.clone());
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for path in parse_library_paths(&text) {
                add(PathBuf::from(path));
            }
        }
    }
    libs
}

/// The `"path"  "..."` values of a `libraryfolders.vdf`, with VDF's `\\` unescaped.
pub fn parse_library_paths(vdf: &str) -> Vec<String> {
    vdf.lines()
        .filter_map(|line| {
            let mut quoted = line.split('"').skip(1).step_by(2);
            (quoted.next()? == "path").then(|| quoted.next().map(|v| v.replace("\\\\", "\\")))?
        })
        .collect()
}

/// The game's install folder in the first library that has it.
pub fn find_install(game: GsiGame, libraries: &[PathBuf]) -> Option<PathBuf> {
    libraries
        .iter()
        .map(|lib| lib.join("steamapps").join("common").join(game.folder()))
        .find(|dir| dir.is_dir())
}

/// What the settings panel shows for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsiTarget {
    pub game: GsiGame,
    pub label: String,
    /// The install folder, or `None` when no Steam library has the game.
    pub install_dir: Option<String>,
    /// Whether the cfg is already in place.
    pub installed: bool,
}

pub fn gsi_targets(libraries: &[PathBuf]) -> Vec<GsiTarget> {
    GsiGame::ALL
        .iter()
        .map(|&game| {
            let install = find_install(game, libraries);
            let installed = install
                .as_ref()
                .is_some_and(|dir| cfg_path(game, dir).is_file());
            GsiTarget {
                game,
                label: game.label().to_string(),
                install_dir: install.map(|p| p.to_string_lossy().into_owned()),
                installed,
            }
        })
        .collect()
}

fn cfg_path(game: GsiGame, install: &Path) -> PathBuf {
    let mut dir = install.to_path_buf();
    dir.extend(game.cfg_dir());
    dir.join(GSI_CFG_NAME)
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsiInstallOutcome {
    pub game: GsiGame,
    pub path: String,
    /// An earlier copy was overwritten.
    pub replaced: bool,
}

/// Write the cfg for `game` into `install` (the game's install folder).
///
/// `port == 0` means the listener is off, so there is nothing for the game to talk to and
/// installing would only mislead; that is refused with the setting's name.
pub fn install_gsi_cfg(
    game: GsiGame,
    install: &Path,
    app_data_dir: &Path,
    port: u16,
) -> Result<GsiInstallOutcome, CommandError> {
    if port == 0 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            "the CS2 / Dota 2 listener is switched off (events.gsi_port = 0), so there is \
             nothing for the game to send to; set a port in config.toml first",
        ));
    }
    // Only the cfg directory of a game that is really installed: a half-deleted CS2 with no
    // `game/csgo` is not somewhere to start creating folders.
    let target = cfg_path(game, install);
    let dest = crate::paths::ensure_new_file_within(install, &target).map_err(|why| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("{}: {why}", target.display()),
        )
    })?;
    let cfg_dir = dest.parent().unwrap_or(install);
    if game == GsiGame::Cs2 && !cfg_dir.is_dir() {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "{} does not exist; is Counter-Strike 2 fully installed?",
                cfg_dir.display()
            ),
        ));
    }
    let token = gsi::load_or_create_token(&app_data_dir.join("gsi-token.txt"))
        .map_err(|e| CommandError::new(ErrorCode::Io, format!("the GSI token: {e:#}")))?;
    std::fs::create_dir_all(cfg_dir).map_err(|e| CommandError::io("creating", cfg_dir, e))?;
    let replaced = dest.is_file();
    std::fs::write(&dest, game.render(port, &token))
        .map_err(|e| CommandError::io("writing", &dest, e))?;
    tracing::info!(
        "installed the {} GSI cfg at {}",
        game.label(),
        dest.display()
    );
    Ok(GsiInstallOutcome {
        game,
        path: dest.to_string_lossy().into_owned(),
        replaced,
    })
}

// ---------------------------------------------------------------------------------------
// The health page
// ---------------------------------------------------------------------------------------

/// How the recorder is, in one word the page can colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    Recording,
    /// Armed for a game, not recording yet.
    Waiting,
    Idle,
    /// Stopped by a failure; `error` says which.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskHealth {
    pub path: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSourceDto {
    pub source: String,
    /// `running`, `disabled` or `failed`.
    pub state: String,
    pub detail: String,
}

/// Mirrored in `src/lib/types.ts`; a test pins the key set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthDto {
    pub recorder_state: RecorderState,
    pub recorder_error: Option<String>,
    /// The encoder the latest recording ran; `None` before any recording.
    pub encoder: Option<String>,
    pub ffmpeg_available: bool,
    pub disk: Option<DiskHealth>,
    /// Empty until a recording starts: the sources live exactly as long as it does.
    pub event_sources: Vec<EventSourceDto>,
    /// Newest last.
    pub recent_errors: Vec<String>,
    pub log_path: String,
}

pub fn recorder_state(status: &RecorderStatus) -> RecorderState {
    if status.running {
        RecorderState::Recording
    } else if status.error.is_some() {
        RecorderState::Failed
    } else if status.watching_games {
        RecorderState::Waiting
    } else {
        RecorderState::Idle
    }
}

pub fn event_source_dtos(reports: &[SourceReport]) -> Vec<EventSourceDto> {
    reports
        .iter()
        .map(|r| EventSourceDto {
            source: r.source.to_string(),
            state: match r.state {
                SourceState::Running => "running",
                SourceState::Disabled => "disabled",
                SourceState::Failed => "failed",
            }
            .to_string(),
            detail: r.detail.clone(),
        })
        .collect()
}

/// Free and total bytes of the disk holding `dir` (its nearest existing ancestor, so a clips
/// directory that does not exist yet still answers).
pub fn disk_health(dir: &Path) -> Option<DiskHealth> {
    let probe = dir.ancestors().find(|p| p.exists())?;
    Some(DiskHealth {
        path: dir.to_string_lossy().into_owned(),
        free_bytes: fs4::available_space(probe).ok()?,
        total_bytes: fs4::total_space(probe).ok()?,
    })
}

/// The last `max` WARN/ERROR lines of a log, oldest first.
///
/// A log line is `<timestamp>  LEVEL target: message`; the level is matched as a whole word
/// so a message that merely contains "error" is not counted.
pub fn recent_problems(log: &str, max: usize) -> Vec<String> {
    let mut found: Vec<String> = log
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .take(3)
                .any(|w| w == "ERROR" || w == "WARN")
        })
        .map(|l| l.trim().chars().take(400).collect())
        .collect();
    let drop = found.len().saturating_sub(max);
    found.drain(..drop);
    found
}

/// Read the tail of the log file (at most 256 KiB) and extract [`recent_problems`].
pub fn read_recent_problems(log_path: &Path, max: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 256 * 1024;
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL)))
        .is_err()
    {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    recent_problems(&String::from_utf8_lossy(&bytes), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_steam(root: &Path, game: GsiGame) -> PathBuf {
        let install = root.join("steamapps/common").join(game.folder());
        std::fs::create_dir_all(&install).unwrap();
        install
    }

    #[test]
    fn library_paths_are_read_from_the_vdf_and_unescaped() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Games\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            parse_library_paths(vdf),
            vec!["C:\\Games\\Steam", "D:\\SteamLibrary"]
        );
    }

    #[test]
    fn a_game_is_found_in_a_secondary_library_listed_by_the_vdf() {
        let steam = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(steam.path().join("steamapps")).unwrap();
        std::fs::write(
            steam.path().join("steamapps/libraryfolders.vdf"),
            format!(
                "\"x\"\n{{\n\"0\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
                second.path().display()
            ),
        )
        .unwrap();
        let install = fake_steam(second.path(), GsiGame::Dota2);
        let libs = steam_libraries(&[steam.path().to_path_buf()]);
        assert_eq!(find_install(GsiGame::Dota2, &libs), Some(install));
        assert_eq!(find_install(GsiGame::Cs2, &libs), None);
        let targets = gsi_targets(&libs);
        assert!(targets[0].install_dir.is_none() && !targets[0].installed);
        assert!(targets[1].install_dir.is_some() && !targets[1].installed);
    }

    #[test]
    fn installing_writes_the_cfg_with_the_port_and_a_stable_token() {
        let steam = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();

        let first = install_gsi_cfg(GsiGame::Cs2, &install, data.path(), 3000).unwrap();
        assert!(!first.replaced);
        let text = std::fs::read_to_string(&first.path).unwrap();
        assert!(text.contains("http://127.0.0.1:3000"), "{text}");
        let token = std::fs::read_to_string(data.path().join("gsi-token.txt")).unwrap();
        assert!(text.contains(token.trim()));

        let again = install_gsi_cfg(GsiGame::Cs2, &install, data.path(), 3000).unwrap();
        assert!(again.replaced, "a second install overwrites and says so");
        assert_eq!(std::fs::read_to_string(&again.path).unwrap(), text);
        assert!(gsi_targets(&[steam.path().to_path_buf()])[0].installed);
    }

    #[test]
    fn dota_gets_its_gamestate_integration_folder_created() {
        let steam = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Dota2);
        let out = install_gsi_cfg(GsiGame::Dota2, &install, data.path(), 3000).unwrap();
        assert!(Path::new(&out.path).is_file());
        assert!(out.path.contains("gamestate_integration"));
    }

    #[test]
    fn a_cs2_install_without_its_cfg_dir_is_refused_and_nothing_is_created() {
        let steam = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        let err = install_gsi_cfg(GsiGame::Cs2, &install, data.path(), 3000).unwrap_err();
        assert!(err.message.contains("fully installed"), "{}", err.message);
        assert!(!install.join("game").exists());
    }

    #[test]
    fn a_disabled_listener_refuses_to_install() {
        let steam = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Dota2);
        let err = install_gsi_cfg(GsiGame::Dota2, &install, data.path(), 0).unwrap_err();
        assert!(err.message.contains("gsi_port"));
        assert!(!data.path().join("gsi-token.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_install_whose_cfg_dir_is_a_symlink_out_of_the_game_is_refused() {
        let steam = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo")).unwrap();
        std::os::unix::fs::symlink(outside.path(), install.join("game/csgo/cfg")).unwrap();
        assert!(install_gsi_cfg(GsiGame::Cs2, &install, data.path(), 3000).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn recorder_state_distinguishes_the_four_conditions() {
        let stopped = RecorderStatus::stopped();
        assert_eq!(recorder_state(&stopped), RecorderState::Idle);
        assert_eq!(
            recorder_state(&RecorderStatus {
                running: true,
                ..stopped.clone()
            }),
            RecorderState::Recording
        );
        assert_eq!(
            recorder_state(&RecorderStatus {
                watching_games: true,
                ..stopped.clone()
            }),
            RecorderState::Waiting
        );
        assert_eq!(
            recorder_state(&RecorderStatus {
                error: Some("boom".into()),
                ..stopped
            }),
            RecorderState::Failed
        );
    }

    #[test]
    fn event_sources_map_to_plain_strings() {
        let dtos = event_source_dtos(&[SourceReport {
            source: "CS2 / Dota 2",
            state: SourceState::Failed,
            detail: "address in use".into(),
        }]);
        assert_eq!(dtos[0].state, "failed");
        assert_eq!(dtos[0].detail, "address in use");
    }

    #[test]
    fn recent_problems_keeps_only_warn_and_error_levels_newest_last() {
        let log = "2026-10-17T10:00:00Z  INFO a: fine\n\
                   2026-10-17T10:00:01Z  WARN a: first\n\
                   2026-10-17T10:00:02Z  INFO a: this mentions ERROR in prose\n\
                   2026-10-17T10:00:03Z ERROR a: second\n\
                   2026-10-17T10:00:04Z  WARN a: third\n";
        assert_eq!(recent_problems(log, 10).len(), 3);
        let last_two = recent_problems(log, 2);
        assert!(last_two[0].contains("second") && last_two[1].contains("third"));
    }

    #[test]
    fn reading_a_missing_log_is_empty_not_an_error() {
        assert!(read_recent_problems(Path::new("/definitely/not/here.log"), 5).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("l.log");
        std::fs::write(&log, "t  ERROR x: broke\n").unwrap();
        assert_eq!(read_recent_problems(&log, 5).len(), 1);
    }

    #[test]
    fn disk_health_answers_for_a_directory_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let health = disk_health(&dir.path().join("not/yet")).unwrap();
        assert!(health.total_bytes >= health.free_bytes && health.total_bytes > 0);
    }

    #[test]
    fn the_health_json_keys_are_the_typescript_interface() {
        let health = HealthDto {
            recorder_state: RecorderState::Idle,
            recorder_error: None,
            encoder: None,
            ffmpeg_available: true,
            disk: None,
            event_sources: vec![],
            recent_errors: vec![],
            log_path: String::new(),
        };
        let value = serde_json::to_value(&health).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "disk",
                "encoder",
                "event_sources",
                "ffmpeg_available",
                "log_path",
                "recent_errors",
                "recorder_error",
                "recorder_state"
            ]
        );
    }
}
