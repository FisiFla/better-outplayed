# Anti-cheat policy

replayed must be safe to run next to kernel anti-cheat (Vanguard, EAC, BattlEye, VAC).
It therefore uses **only** the capture paths OBS-style recorders use:

- Windows Graphics Capture (video) and WASAPI loopback / microphone (audio),
- `RegisterHotKey` for the clip hotkey,
- read-only, official game APIs: CS2 / Dota 2 Game State Integration (the game POSTs to us) and
  the League Live Client Data API (`https://127.0.0.1:2999`, GET only).

It never injects, hooks, reads or writes another process's memory, synthesises input, or writes
to a game client. `scripts/check-forbidden-apis.sh` enforces the API half of this in CI.

**The one request it makes to another machine.** *Settings → Updates → Check for updates*
asks this project's own GitHub release page whether there is a newer replayed. That is an
ordinary HTTPS GET, and then a download of the installer from the same page; it touches no
game, no game client and no process, it says nothing about the machine it runs on, and it
happens only when the user presses the button. Nothing else in the application reaches the
network. What it downloads is checked against the signing key this build was made with
before it is installed.

Tested anti-cheats: none systematically yet — tracked in the release checklist. This project is not
affiliated with any game or anti-cheat vendor; use at your own risk.

## Operating rules for testing on a machine that runs anti-cheat

These apply to anyone (human or automated agent) verifying this project on a Windows machine
where a kernel anti-cheat such as Riot Vanguard is installed:

- **Never synthesise input.** No `SendInput`, `keybd_event`, SendKeys or AutoHotkey; ask the
  person at the machine to press keys physically. The CLI's `--self-test-clip-after` trigger
  exists so verification does not need a keypress.
- **Never write to a game client.** The League Live Client Data API is GET-only; never POST or
  PUT to the League client (LCU) API.
- **No injection, hooks or game-memory access** of any kind.
- **No screen capture while a protected game is running.** Check that the game and its client
  processes (for League of Legends: `League of Legends`, `LeagueClient`, `LeagueClientUx`) are
  not running before any run that touches the display.
- `cargo build`, `cargo test` and `cargo xtask probe` are CPU-only and safe to run at any time.
- From a remote shell (no desktop session), run GUI or capture steps through a one-shot
  scheduled task in the logged-on user's session, and clean the task up afterwards.
- Read the application log rather than asking for a screenshot.
- Clean up when finished: scheduled tasks, helper scripts and scratch build directories.
