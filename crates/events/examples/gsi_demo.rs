//! A runnable demonstration of the GSI listener with **no game anywhere near it**.
//!
//! ```text
//! CARGO_HOME="$PWD/target/cargo-home" cargo run -p localplay-events --example gsi_demo -- --seconds 30
//! ```
//!
//! It prints the token it generated, the `gamestate_integration_localplay.cfg` it would ask
//! a player to install, and a `curl` command that POSTs a realistic Counter-Strike payload
//! to it. Then it prints every event its derivation produces from what arrives, until
//! `--seconds` elapse.
//!
//! What this demonstrates and what it does not: the listener, the request gate, the token
//! comparison, the payload parse and the whole derivation are the real ones, driven by a
//! real HTTP POST over loopback. What is not demonstrated is a game: the payload comes from
//! `crates/events/tests/fixtures/gsi_cs2_competitive.json` or from your `curl`, and no CS2 or
//! Dota 2 client is involved at any point. The token printed here is ephemeral and is not
//! written anywhere.
//!
//! Exits on its own (there is no `timeout(1)` on macOS): the deadline is the point of the
//! `--seconds` flag.

use localplay_events::gsi::{self, GsiConfig};
use localplay_events::{EventKind, GameEvent};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

fn main() {
    let mut port: u16 = 0;
    let mut seconds: u64 = 20;
    let mut token: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--port" => port = value.parse().expect("--port takes a number"),
            "--seconds" => seconds = value.parse().expect("--seconds takes a number"),
            "--token" => token = Some(value),
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("usage: gsi_demo [--port <port>] [--seconds <n>] [--token <token>]");
                std::process::exit(2);
            }
        }
        i += 2;
    }

    // A token for this run only. The application keeps its own in the app data directory
    // (`gsi::load_or_create_token`); a demo that outlives its own process should not write
    // anything.
    let token = token.unwrap_or_else(|| gsi::generate_token().expect("the OS provides randomness"));

    let (sink, events) = channel();
    let listener = match gsi::spawn(GsiConfig::new(port, token.clone()), sink) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("the listener could not start: {err:#}");
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr();
    let uri = format!("http://127.0.0.1:{}{}", addr.port(), gsi::DEFAULT_PATH);

    println!("== the listener ==");
    println!("bound to {addr} (loopback only; a non-loopback address is refused)");
    println!("POST path: {}", gsi::DEFAULT_PATH);
    println!("token:     {token}");
    println!();
    println!("== the file a player would install ==");
    print!("{}", gsi::integration_cfg(addr.port(), &token));
    println!();
    println!("== drive it ==");
    println!("A payload must carry this run's token in its \"auth\" block. Either edit a fixture,");
    println!("or start this demo with --token fixture-token-not-a-secret so that the committed");
    println!("fixtures can be posted exactly as they are:");
    println!();
    println!("  curl -sS -o /dev/null -w 'HTTP %{{http_code}}\\n' -X POST \\");
    println!("       --data-binary @crates/events/tests/fixtures/gsi_cs2_competitive.json \\");
    println!("       {uri}");
    println!();
    println!("A payload whose token is missing or different is answered 403 and derives nothing.");
    println!();
    println!("== derived events ==");

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut seen = 0usize;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                seen += 1;
                report(seen, &event);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!();
    if seen == 0 {
        println!("no payload arrived in {seconds}s — nothing was derived (which is itself the \
                  answer for an idle listener: it does nothing on its own)");
    } else {
        println!("{seen} event(s) derived from the payloads that arrived");
    }
    listener.stop();
    println!("the listener stopped; it is bound to nothing now");
}

fn report(n: usize, event: &GameEvent) {
    let kind = match event.kind {
        EventKind::GameStart | EventKind::GameEnd => "lifecycle",
        k if k.is_highlight() => "HIGHLIGHT (a clip would be taken here)",
        _ => "marker (recorded, no clip)",
    };
    println!(
        "{n:>3}. {:<14} {:<36} {kind}",
        event.kind.to_string(),
        event.source.to_string()
    );
    if let Some(payload) = &event.payload {
        println!("     {payload}");
    }
}
