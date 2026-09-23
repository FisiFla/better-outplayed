//! localplay CLI — the Phase 1 headless replay-buffer PoC.
//!
//! Thin by design: logging setup, subcommand dispatch, and nothing else. The pipeline
//! itself lives in `localplay-recorder` — the engine the desktop shell drives too — and
//! `localplay_cli` is the driver around it: parse the config, start a `Recorder`, install
//! the hotkey, clip on a press. It is a library rather than logic in this file so that
//! integration tests can reach it (`apps/localplay-cli/tests/post_roll.rs` in particular —
//! the hotkey that reaches this path is Windows-only, so no test could otherwise execute
//! it), and so that `--self-test-clip-after` can be driven in-process by a test as well.
//!
//! `--help` prints the flag list, including the verification-only self-test trigger, and
//! says there in the help text that it sends no keyboard or mouse input: "does this touch
//! my input?" is the first question a diagnostic that takes a clip has to answer.

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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match args.first().map(String::as_str) {
        Some("buffer") => localplay_cli::run_buffer(),
        Some(other) => bail!("unknown subcommand: {other}\n\n{}", localplay_cli::help()),
        None => bail!("{}", localplay_cli::help()),
    }
}
