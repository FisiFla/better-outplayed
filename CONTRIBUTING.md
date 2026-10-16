# Contributing

## Build and test
```
cargo test --workspace --features localplay-encoder/test-encoders   # needs ffmpeg on PATH
cd apps/desktop && npm ci && npm run check && npm test && npm run build
cd apps/desktop/src-tauri && cargo test                              # separate workspace
cargo clippy --workspace --all-targets -- -A clippy::unwrap_used -D warnings
scripts/check-forbidden-apis.sh                                      # anti-cheat guard
```
On macOS the capture path is a synthetic stub: green tests prove nothing about Windows capture.
Read `docs/verification-status.md` for what has actually run on Windows hardware.

## Rules
- The five principles in the README are constraints, not preferences: no cloud, no telemetry, no
  Overwolf, minimal resources, lossless trims.
- Never add APIs that touch another process or synthesise input (`docs/anti-cheat.md`).
- Windows-only code must at least pass `cargo check --target x86_64-pc-windows-msvc` (CI does this).
- Security issues: see `SECURITY.md`; do not file them publicly.
- PowerShell eats `--`; call the Tauri CLI binary directly when bundling (`docs/packaging.md`).
