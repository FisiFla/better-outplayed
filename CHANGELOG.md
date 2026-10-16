# Changelog

All notable changes to better-outplayed. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). No release has been tagged yet, so
everything is under *Unreleased*; the dated headings below are when the work landed, not
versions. For what has and has not been *verified* on real hardware, read
[`docs/verification-status.md`](docs/verification-status.md), not this file.

## [Unreleased]

### Added
- **Health page** in the desktop window: recorder state, the encoder the last recording ran,
  free disk space where clips are stored, how each game-event source started, and the last
  warnings and errors from the log.
- **Game integration installer** in Settings: finds Counter-Strike 2 and Dota 2 in the Steam
  libraries and writes `gamestate_integration_localplay.cfg` into the game's `cfg` folder when
  you press Install. The webview only names the game; the destination is checked to stay
  inside the game's install folder (symlinks and `..` refused).
- **Privacy section** in the README: what is recorded, that it stays local, and that there is
  no telemetry.
- This changelog.
- Opt-in `toggle_recording` and bookmark hotkey chords, with a Bookmark marker kind (2026-10-17).
- Toast and sound when a clip is saved, from any trigger (2026-10-17).
- `[events] mute_clips`: keep chosen highlight kinds as timeline markers instead of clips
  (2026-10-16).
- The game-event sources (League, CS2 / Dota 2) now run inside the desktop app, with burst
  coalescing (2026-10-16).
- Backup of the clip database before a migration; `SECURITY.md`, an anti-cheat policy with an
  API guard, and a release checklist (2026-10-16).

### Changed
- `RecorderStatus` reports the encoder in use; the event driver reports how each source
  started (`SourceReport`).
- `docs/verification-status.md` and the README no longer say the desktop GUI or
  `cargo xtask verify` have never run on Windows (they ran 2026-09-26 and 2026-10-15), and the
  ledger tally was recounted from the item tables (53 distinct claims).

### Fixed
- Dependabot bumps applied together with the code changes they needed (2026-10-16).

## 2026-10-15 - encoder and capture rate honesty
- The configured video and audio bitrates are now actually sent to the encoder; the rate is set
  through `ICodecAPI`, not only described.
- `[audio] enabled = false` gives a true video-only recording.
- The ring reports where its footage starts instead of assuming zero.

## 2026-09-26 - diagnosable desktop app
- The app writes a rotating log file and a panic hook; frontend errors go to the same log.
- Subprocesses no longer open console windows.
- `list_clips` omits clips whose file is gone and `thumbnail` refuses them: a stale index had
  caused an ffmpeg storm.
- Settings panel (library folder with a Browse button, framerate, resolution, microphone,
  auto-record).
- Windows installer configured and measured; signing path prepared for when a certificate exists.

## 2026-09-25 - zero-copy capture
- Hybrid GPU video path removes the CPU readback (CPU per 4K capture from ~50% of a core to
  ~3%); the hardware encoder runs on its own thread.
- Project renamed to better-outplayed.

## 2026-09-24 - first Windows runs
- Whole workspace built and tested on Windows; first real capture, clip and hardware encode.
- In-memory replay buffer; game presence through process names and Riot's Live Client API.

## 2026-09-23 - initial build
- Phases 0-4: workspace, lossless media tools, ring buffer and clip splicing, capture backends,
  hardware-encoder selection with smoke test, SQLite index and storage policy, CLI, Tauri
  desktop shell with tray and global hotkey, LoL and CS2 / Dota 2 event integrations, sidecar
  fetching with checksums, CI.
