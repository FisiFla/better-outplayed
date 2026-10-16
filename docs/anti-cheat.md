# Anti-cheat policy

better-outplayed must be safe to run next to kernel anti-cheat (Vanguard, EAC, BattlEye, VAC).
It therefore uses **only** the capture paths OBS-style recorders use:

- Windows Graphics Capture (video) and WASAPI loopback / microphone (audio),
- `RegisterHotKey` for the clip hotkey,
- read-only, official game APIs: CS2 / Dota 2 Game State Integration (the game POSTs to us) and
  the League Live Client Data API (`https://127.0.0.1:2999`, GET only).

It never injects, hooks, reads or writes another process's memory, synthesises input, or writes
to a game client. `scripts/check-forbidden-apis.sh` enforces the API half of this in CI.

Tested anti-cheats: none systematically yet — tracked in the release checklist. This project is not
affiliated with any game or anti-cheat vendor; use at your own risk.
