# Changelog

All notable changes to replayed. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). No release has been tagged yet, so
everything is under *Unreleased*; the dated headings below are when the work landed, not
versions. For what has and has not been *verified* on real hardware, read
[`docs/verification-status.md`](docs/verification-status.md), not this file.

## [Unreleased]

### Added
- **Game audio by process loopback** (`83eaa0c`): the game track now records only the
  watched game's own audio — WASAPI process loopback on that game's process id — so voice
  chat, music and notifications stay out of the clip. **Privacy-relevant**: the default is
  narrower than the old "everything the speakers play", and it falls back to the whole
  output only while no watched game is running. `[capture] game_audio = "system"` restores
  the old behaviour.
- **Per-match segments and a match timeline** (`dd77671`): a session is split into per-match
  segments, the sessions view plots the matches in it on a timeline, and a clip can be cut
  out of one match rather than the whole session. The League end-of-game scoreboard is
  captured with it.
- **Game events as MP4 chapters** (`0f39d22`): the events inside a clip's span are written
  into the file as chapters, so they are visible in the clip itself and not only in the
  library.
- **Schema v5 library** (`de2051c`): a clip carries a title, a game and tags, the library
  view searches and filters on them, and a clip can be copied, revealed or dragged out.
- **Replay buffer plus full session off one encode**: `[recorder] save_session = true` (Settings →
  "Also save the full session") writes the whole recording to a session file while the buffer keeps
  serving clips, with no second encoder. The file is written as fragments arrive, so a crash loses
  about a second and the next start recovers it; on stop it is remuxed into the usual
  `session-<timestamp>.mp4`. A disk that cannot keep up ends the session file early with a warning and
  never the buffer. Covered by tests against the software encoder; unverified on hardware.
- **Audio device selection**: Settings now picks the microphone (`[mic] device`) and the output
  device recorded as desktop audio (`[capture] output_device`) from the active Windows endpoints;
  empty keeps the Windows default. A chosen device that is unplugged at start refuses the
  recording instead of recording another one. The "Microphone track" checkbox is now "Record
  microphone". Windows-only paths type-check but are unverified on hardware.
- **Desktop UI overhaul**: a navigation rail (Library, Sessions, Storage, Health, Settings), a
  clip-card grid, per-view pages, Inter / JetBrains Mono, Lucide icons and a role-named dark
  token set. Polish: view cross-fades, Esc / Alt+Left to leave a clip page, toasts that
  dismiss themselves (6 s notices, 12 s errors), one empty-state style, `prefers-reduced-motion`
  honoured (and set in the screenshot run). README screenshots are refreshed from `npm run shots`.
  **Unverified:** everything was rendered in headless Chromium against fixtures; WebView2, real
  DPI scaling (150%) and Inter's variable-font rendering on Windows have not been seen.
- **Health page** in the desktop window: recorder state, the encoder the last recording ran,
  free disk space where clips are stored, how each game-event source started, and the last
  warnings and errors from the log.
- **Game integration installer** in Settings: finds Counter-Strike 2 and Dota 2 in the Steam
  libraries and writes `gamestate_integration_replayed.cfg` into the game's `cfg` folder when
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
- **The bundled ffmpeg is now an LGPL build.** The pin was Gyan's *essentials* archive, which
  upstream's own page says is **GPLv3** and which carries **libx264 and libx265** — while the
  README and the pin file said it was LGPL and that "LGPL is sufficient". The pin now points
  at a versioned BtbN release asset whose configure line carries `--disable-libx264
  --disable-libx265` and no `--enable-gpl`, with its SHA-256 recorded in
  `xtask/sidecars.toml`. A versioned asset also stops the pin breaking on every upstream
  rebuild. `apps/desktop/THIRD_PARTY_NOTICES.md` now names the bundled ffmpeg, the Tauri
  runtime, the npm packages and the Rust crates, and ships with the installer.
- **CI now lints the code that actually ships.** The clippy step's `-A clippy::unwrap_used`
  was overriding the workspace's own `[lints]` table, so the `unwrap_used` guarantee its
  comment claimed CI enforced was enforced nowhere. The two blanket `-A` flags are gone, the
  test bodies are exempted structurally instead, and there is a **Windows-target** clippy step
  — 52 `cfg(windows)` blocks across 16 files had never been linted at all. `cargo fmt --check`
  and a `cargo xtask verify` run now happen in CI too, and every action is pinned to a full
  commit SHA.
- **`cargo xtask verify` gained `--strict`**, which makes a *not performed* check a non-zero
  exit, and its verdict line now reads `PASS with N check(s) not performed` instead of a bare
  `PASS`. The harness's two ring rows were rewritten to match the engine: the cap row is
  `ram_cap` (against `buffer.ram_cap_bytes`, the bound the engine really enforces) and the
  eviction row reads the ring's own `evicted=` counter instead of looking for segment files
  that buffer mode never writes.
- The desktop shell no longer exits on a malformed `config.toml`: it starts with the example's
  values and says why, in the window. The global hotkey is now installed **after** the
  single-instance check, so a double launch cannot leave the survivor with a dead hotkey. The
  six blocking commands (`trim_clip`, `thumbnail`, `copy_clip_to_clipboard`, `delete_session`,
  `list_audio_devices`, `update_settings`) run off the UI thread, so the window no longer
  freezes for every trim, thumbnail and clipboard copy. The clips directory can only be
  changed to a folder the **user** picked, through a native folder picker the backend runs —
  the webview can no longer widen the asset protocol's read scope to any path it names.

### Fixed
- **The game state integration can no longer be squatted, go stale, or be forged with another game's token.** Each game now has its own token and its own path, and the listener refuses a payload whose provider app id is not that game's, so a token lifted from one game's installed `.cfg` cannot forge the other game's events. The listener is bound before any `.cfg` is written, and every `.cfg` -- in the app's data directory and in the game's own folder -- names the port it actually bound rather than the configured one. The desktop shell no longer re-reads the token file on its own: the installed `.cfg` is written from the live binding, it is written atomically, it is refreshed when the listener's port moves, and an install is refused outright when no listener is running. `[events] gsi_ephemeral_port` opts into an unguessable port for anyone who does not need a fixed one. The old single token file is migrated into the per-game files, so an existing install keeps working.

- **The screenshot harness drives the whole clip-source contract, and the build now checks
  that it does.** The harness's fake `ClipSource` had fallen behind the interface: the
  method that opens the native folder picker was missing, so the settings screenshot
  failed with `chooseClipsDir is not a function`, and seven further methods were missing
  besides. The harness was not a CI gate, so the README's "0 failures" went stale instead
  of turning red. Every method the interface declares is now implemented, a new test
  (`clip-source-contract.test.ts`) fails the build when the two fall apart again, the
  screenshot run is a CI step, and the settings panel's Browse button treats a picker
  that is missing or throws as "no directory" rather than an uncaught error.
- **The secret scan now covers the whole history, not just the commits a push brought.** The
  per-push gitleaks step narrows its scan to the range the push touched, so a credential
  committed long ago was never reported — and this repository carried a committed TLS test
  key from 2026-09-23 to 2026-09-26 without a scan ever mentioning it. A weekly
  `secret-scan` workflow now sweeps every commit, and the two documented exceptions (fixed
  hex strings in the Game State Integration tests, and that removed test key) live in
  `.gitleaks.toml` with the reason written down, instead of in a `.gitleaksignore`
  fingerprint that goes stale when a line moves.
- **The webview can no longer forge log lines.** The frontend's own log command flattened its
  input to one line and capped it, and every frontend line is tagged `[webview]`, so a newline
  in the text can no longer end the log line and have the rest of it read as a backend entry.
- Dependabot bumps applied together with the code changes they needed (2026-10-16).
- **A failed recording start no longer bricks games mode.** The recorder took its single
  SQLite connection out of the struct before the fallible half of the start handshake and only
  put it back on the success path, so one transient failure — a probe timeout, a locked
  database, a WGC session held by another app — left auto-record dead for the life of the
  process, reporting the misleading message *"this recorder's store is already in a
  recording"*.
- **A clip no longer punches a hole in the recording it was cut from.** The ffmpeg splice ran
  on the pump thread, so the capture pump stopped for the length of the remux and the encoder
  dropped the frames it missed. The splice now runs on its own thread and the pump keeps
  going.
- **Event markers and MP4 chapters are stamped at the moment the event happened**, not when
  the command was handled — a burst-coalesced kill was landing up to 18 seconds late — and
  chapter offsets are derived from the clip's real first frame rather than the requested
  window start.
- **The post-roll wait no longer loses clips.** The budget ignored the session ledger's
  one-segment trust lag, so with `segment_time >= 6` every clip timed out and was lost.
- **The index no longer loses a clip to a locked database.** SQLite had no busy timeout
  anywhere, and two connections write to the same file, so a hotkey press racing a UI write
  could leave a clip on disk with no row — invisible in the library and unmanaged by the
  storage policy.
- The ffmpeg child is now killed **and reaped** when an encoder is dropped, instead of
  leaving an unreaped process and three detached writer threads.
- Durations derived from configuration and file names are now saturating, so an absurd
  `pre_seconds` cannot wrap a timestamp.
- The GSI listener now checks the token **before** parsing the body, and serves connections
  from a small pool instead of one at a time — a browser page could previously make it parse
  2 MB of JSON per request on a single thread and hold the real game's events off the socket.
- The zero-egress guard is a capability denylist with an explicit per-file exemption list
  rather than a three-literal allowlist, so `reqwest::get`, `ureq::get` and
  `Command::new("curl")` no longer pass it unnoticed.
- The anti-cheat guard covers the APIs the policy names but the script did not
  (`OpenProcess`, `LoadLibraryEx`, the `Nt`/`Zw` memory calls, `DeviceIoControl`,
  `PostMessage`, `GetProcAddress`, and more) and now scans `scripts/` as well.
- The Timeline's *"click a marker to seek to just before it"* actually seeks now; the press
  bubbled up to the track and clobbered the marker, so the feature was dead at runtime. The
  trim handles answer `Home`/`End`/`PageUp`/`PageDown`, the two extraction buttons agree,
  and an overlapping library refresh can no longer resurrect a stale list.
- `escape_ffmetadata` escapes `\r` as well: a carriage return in a chapter title could inject
  a `title=` line into the next chapter.
- The desktop's autostart entry is removed even when the registry query fails, and a `reg`
  failure is no longer read as "the value is not set".
- The README, the ledger, the runbooks, the packaging notes and `apps/desktop/README.md` were
  corrected against the code: the status block, the TLS story, the ring buffer, the DXGI
  fallback, the encoder path, the audio source, the CI description, 25 commit hashes that did
  not exist in this history, and the "there is no Windows runner" claim.

## 2026-10-15 - encoder and capture rate honesty
- The configured video and audio bitrates are now actually sent to the encoder; the rate is set
  through `ICodecAPI`, not only described (superseded on 2026-10-17: the MFT still overshoots;
  see ledger §19 and §24).
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
- Project renamed to replayed.

## 2026-09-24 - first Windows runs
- Whole workspace built and tested on Windows; first real capture, clip and hardware encode.
- In-memory replay buffer; game presence through process names and Riot's Live Client API.

## 2026-09-23 - initial build
- Phases 0-4: workspace, lossless media tools, ring buffer and clip splicing, capture backends,
  hardware-encoder selection with smoke test, SQLite index and storage policy, CLI, Tauri
  desktop shell with tray and global hotkey, LoL and CS2 / Dota 2 event integrations, sidecar
  fetching with checksums, CI.
