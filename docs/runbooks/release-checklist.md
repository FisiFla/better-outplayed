# Release checklist

Tick every box on the release candidate; attach the filled copy to the GitHub Release.

## Automated (must be green on the tagged commit)
- [ ] CI: macOS tests, Windows tests, supply-chain (cargo-deny, npm audit, gitleaks, API guard)
- [ ] Installer builds; contains `binaries\ffmpeg.exe` / `ffprobe.exe` (LGPL build, no `--enable-gpl`)

## Windows hardware (human, interactive session — see `docs/anti-cheat.md` for the Vanguard operating rules)
- [ ] `cargo xtask verify` report attached; first row `capture_started` is real 3840x2160 (or native) capture
- [ ] Install from the NSIS artifact; first run; tray icon; hide-on-close
- [ ] Clip hotkey pressed by a person -> clip appears, plays, scrubs, trims, deletes
- [ ] Settings: folder picker, fps, mic toggle, start-with-system (check `HKCU\\...\\Run`, reboot)
- [ ] CSP: no console violations in the review window; thumbnails and video load
- [ ] Upgrade over the previous release keeps clips, settings, Run key
- [ ] Uninstall behaves as the installer text says
- [ ] 72 h soak: flat RSS, bounded `logs\\` directory, no console windows
- [ ] CS2 / Dota 2 GSI and League Live Client events against a real game (read-only APIs only)
- [ ] Anti-cheat run-alongside check recorded in docs/anti-cheat.md

## Housekeeping
- [ ] CHANGELOG, version bumped in tauri.conf.json / Cargo.toml / package.json
- [ ] docs match reality (README status, verification ledger)
