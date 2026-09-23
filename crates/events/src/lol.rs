//! League of Legends — Live Client Data API (spec §7.1).
//!
//! Riot's game client serves a JSON snapshot of the running game on
//! `https://127.0.0.1:2999/liveclientdata/allgamedata` and stops serving it when the game
//! is over. This module polls it about once a second and derives [`GameEvent`]s from the
//! difference between consecutive snapshots.
//!
//! # Layout: pure logic first, I/O at the edge
//!
//! | file | what it owns | I/O |
//! |---|---|---|
//! | [`endpoint`] | where the API lives, and the type that cannot point anywhere else | none |
//! | [`payload`] | the response document as typed Rust | none |
//! | [`derive`] | snapshot → events, as a pure state machine | none |
//! | [`client`] | the one HTTPS client, pinned to the endpoint | sockets |
//! | [`poller`] | the loop, the logging discipline, the event sink | sockets |
//!
//! The split is what makes the interesting half testable: `derive` is covered against
//! canned payloads in `crates/events/tests/fixtures/`, and the network layer is exercised
//! against a local HTTPS mock server (`tests/lol_mock.rs`) that speaks the same dialect as
//! Riot's — a self-signed certificate on `127.0.0.1`.
//!
//! # What is *not* verified, and cannot be from this machine
//!
//! No League client was contacted while writing any of this, by requirement: the game runs
//! with kernel-level anti-cheat and the machine it is installed on was not available. The
//! payload shapes here come from the documented API and are treated as untrusted input —
//! every field is optional and no payload, however malformed, can panic the parser. What
//! that leaves unproven is written down in `docs/plans/2026-09-23-localplay-phase-4-integrations.md`.
//!
//! # The security arrangement, in one place
//!
//! The endpoint is a loopback address *by type*: [`Endpoint::loopback`] refuses anything
//! that is not, and there is no other constructor. The client that skips TLS verification
//! ([`client::LoopbackClient`]) is built from an `Endpoint` and from nothing else, so a
//! relaxed connector can only ever be pointed at this machine. `tests/no_egress.rs`
//! asserts statically that `danger_accept_invalid_certs` appears in exactly one file in
//! the tree — `lol/client.rs` — so the relaxation cannot spread to a general-purpose
//! client without failing the suite.

pub mod client;
pub mod derive;
pub mod endpoint;
pub mod payload;
pub mod poller;

pub use client::{FetchError, LoopbackClient, Response};
pub use derive::Tracker;
pub use endpoint::Endpoint;
pub use payload::GameSnapshot;
pub use poller::{spawn, LolConfig, LolHandle, LolPoller, POLL_INTERVAL};
