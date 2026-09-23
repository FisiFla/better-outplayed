//! localplay CLI — the Phase 1 headless replay-buffer PoC.
//!
//! Thin by design: logging setup, subcommand dispatch, and nothing else. The pipeline
//! itself lives in the `localplay_cli` library so that integration tests can drive it
//! (`apps/localplay-cli/tests/post_roll.rs` in particular — the hotkey that reaches
//! this path is Windows-only, so no test could otherwise execute it).

use anyhow::{bail, Result};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("buffer") => localplay_cli::run_buffer(),
        Some(other) => bail!("unknown subcommand: {other}\nusage: localplay-cli buffer"),
        None => bail!("usage: localplay-cli buffer"),
    }
}
