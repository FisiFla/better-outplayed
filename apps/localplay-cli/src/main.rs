//! localplay CLI: the headless replay-buffer front-end.
//!
//! Thin by design: logging setup, subcommand dispatch, nothing else. The pipeline lives in
//! `localplay-recorder`, and `localplay_cli` is the driver library around it, so integration
//! tests can reach it (notably `tests/post_roll.rs`, since the hotkey is Windows-only) and
//! drive `--self-test-clip-after` in-process.
//!
//! `--help` lists the flags, including the self-test trigger, and states there that it sends no
//! keyboard or mouse input.

use anyhow::{bail, Result};

fn main() -> Result<()> {
    // Before the tracing subscriber: `--help` is not a run and should print nothing else.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", localplay_cli::help());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match args.first().map(String::as_str) {
        Some("buffer") => localplay_cli::run_buffer(),
        Some(other) => bail!("unknown subcommand: {other}\n\n{}", localplay_cli::help()),
        None => bail!("{}", localplay_cli::help()),
    }
}
