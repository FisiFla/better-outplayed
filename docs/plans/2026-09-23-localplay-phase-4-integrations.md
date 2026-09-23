# Phase 4 — game event integrations

**Status: implemented and tested without a game; not yet run against one.**

This document is the record of what Phase 4 (spec §7) delivered: the two local event
sources, how they are wired into clipping, what is *proven* by the test suite on this
machine, and what can only be proven on a machine with the games installed. Read
[What is not verified](#what-is-not-verified) before believing any of it.

The sources are:

| source | protocol | module |
|---|---|---|
| League of Legends | HTTPS `GET` of `https://127.0.0.1:2999/liveclientdata/allgamedata`, about once a second | `crates/events/src/lol/` |
| CS2 / Dota 2 | an HTTP `POST` listener on `127.0.0.1` (Valve Game State Integration) | `crates/events/src/gsi/` |

Both are **loopback-only by construction**. That is not a policy statement: `lol::Endpoint`
cannot be built for a non-loopback address, `gsi::bind` refuses one, and
`crates/events/tests/no_egress.rs` asserts statically that no other file in the tree
constructs an HTTP client, connects a socket, or relaxes TLS verification.

---

## 1. League of Legends — the Live Client Data API

`lol::Endpoint::live_client()` is `127.0.0.1:2999/liveclientdata/allgamedata`, fixed by
spec §7.1. `lol::client::LoopbackClient` is the only thing that talks to it.

**The TLS arrangement.** Riot serves this endpoint with a self-signed certificate, so there
is nothing that *can* be verified: no name to match, no authority to trust. The spec's
constraint is therefore not "skip verification" but "skip it only for a client that cannot
address anything else", and that is what the code is:

* the relaxation is two builder calls inside `lol/client.rs`, on a `TlsConnector` stored as
  a private field of `LoopbackClient`;
* the client's destination is an `Endpoint`, and an `Endpoint` can only be built for a
  loopback address (`Endpoint::loopback` returns an error otherwise). The relaxation and the
  address are the same object, so a relaxed connector cannot be pointed at another host even
  by a future caller;
* `no_egress.rs` asserts that `danger_accept_invalid_certs` appears in **exactly one file** in
  the tree, and that the file is that one;
* the LoL mock test asserts both directions: a stock client with verification on **refuses**
  the mock's self-signed certificate, and localplay's client accepts it.

`native-tls` was chosen over a bundled TLS stack because it uses the platform's own TLS
(Schannel on Windows, Security.framework here), adds no C build step — so
`cargo check --target x86_64-pc-windows-msvc` still works from macOS, which `rustls` + `ring`
does not — and its `danger_accept_invalid_certs` is the platform-supported way to accept an
unknown authority on both targets.

**The attach policy**, stated because it is the difference between a usable integration and
an event storm: the endpoint serves the whole event log of the game so far, and a game is
usually well under way before localplay starts. So

| transition | emitted |
|---|---|
| no game → game | `GameStart` only; every event already in the document is adopted as seen, and the count is recorded in the payload as `adopted_in_progress_events` |
| game → no game | `GameEnd`, once, on the falling edge (a 404 or a refused connection is the normal end-of-game signal) |
| game → game | only events whose `EventID` has not been seen |
| a *different* game starts without the endpoint going quiet | the same as an attach: `GameStart` and an adoption. Recognised by the game clock moving **backwards** (event ids restart per game and cannot be used for this) |

Idempotence comes from a seen-set of event ids; a document that omits `EventID` falls back to
a content hash of the event, so it is still emitted once.

**The mapping** (`lol/derive.rs`, and every case is a unit test): `ChampionKill` becomes
`Kill` / `Death` / `Assist` by comparing `KillerName` / `VictimName` / `Assisters` against the
names the payload gives the local player — and a champion kill involving nobody it names is
**not** emitted, because a fight the player was not in is not a clip. When the payload names
no player at all (the loading screen), a kill is emitted as `Kill` with both names in its
payload rather than dropped. Dart objectives (`DragonKill`, `BaronKill`, `HeraldKill`,
`EpicMonsterKill`, `HordeKill`, `TurretKilled`, `InhibKilled`, `FirstBrick`, `FirstBlood`,
`Ace`, `Multikill`) are `Objective` regardless of who did them.

**Normal states are not errors.** A refused connection or a 404 (no game running, no League
installed) is logged once at `debug`, not once a second at `error`. A body that cannot be
parsed is a `warn` once per run of them, and it does **not** end the game: the tracker keeps
its state, because one unreadable poll is not evidence that a game stopped.

---

## 2. CS2 / Dota 2 — Valve Game State Integration

`gsi::spawn` binds `127.0.0.1:<events.gsi_port>` and consumes the JSON the game POSTs.

**Binding.** `gsi::bind` refuses any address that is not loopback, and `bind_loopback(port)` —
the only constructor production uses — takes a port and nothing else. There is no
configuration setting for a bind address, so a listener on every interface cannot be
expressed. `[events] gsi_port = 0` leaves the listener off.

**The token gate.** The token arrives **inside the payload**, in the `auth` object: that is
how Valve's `"auth" { "token" "…" }` block works — the game echoes the block's fields into
the POST body as JSON string fields. GSI implements no `Authorization` header and no bearer
scheme, so neither does the listener. A payload whose `auth.token` is missing, shorter,
longer, or one byte different from the generated token is answered `403` and dropped without
being derived; the comparison is constant time (`subtle`), and an empty configured token
fails closed rather than accepting an empty presented one.

**The file the user installs.** `gsi::integration_cfg` (CS2) and `gsi::integration_cfg_dota`
return the *text* of `gamestate_integration_localplay.cfg` — the uri with the configured
port, the generated token, the data blocks each game should subscribe, and `//` comments
(the KeyValues format supports them, and Valve's own sample file uses them). The application
never writes into a game's installation directory: the CLI writes the file into its own data
directory (`<app data>\gsi\cs2\` and `<app data>\gsi\dota2\`) and logs where to copy it from.
The token itself lives in `<app data>\gsi-token.txt` (created `0600` where the platform has
modes), generated on first use and reused afterwards, so an installed file keeps working.

**The mapping** (`gsi/derive.rs`): kills/deaths/assists from the scoreboard deltas, round
start from `map.round` increasing, round end from `round.phase` becoming `"over"`, bomb
planted/defused from the bomb state, and game start/end from `map.phase` (`gameover`) or
Dota's `DOTA_GAMERULES_STATE_*`. Three rules make it survive real payloads:

* **deltas are per player.** The baseline is only compared when the `steamid` matches, so a
  spectator switching point of view re-baselines silently instead of reporting somebody
  else's kills as a jump;
* **a jump is one event.** Two kills inside one POST window are one `Kill` event carrying
  `"delta": 2`, not two clips for something that was never separately observed;
* **transitions, not values.** Round and bomb events fire on the edge, so the heartbeat POST
  — which repeats the state verbatim — emits nothing.

Dota 2's `player` block is an array in observer mode and an object in a player's own client;
both shapes parse, the first entry is used, and the `steamid` rule above means a wrong guess
produces no events rather than a burst of them.

---

## 3. How an event becomes a clip

An event is **not** a second trigger mechanism. A driver hands a derived event to the
recorder, and the recorder takes the clip through the same call a hotkey press takes
(`Recorder::clip_now_with`, which is `Recorder::clip_now` plus a reason):

* the trigger instant is the **ledger's media time**, taken inside the pump loop — the
  property that makes the post-roll reachable on hardware whose media clock runs slower than
  real time;
* the window is `buffer.pre_seconds` before it and `buffer.post_seconds` after it, spliced
  losslessly from whole segments, exactly as for a hotkey clip;
* what the reason adds is one row in the `events` table (spec §5.5), whose `at` is that same
  media instant (so a marker is `pre_seconds` into its clip — the moment that caused it, not
  the start of the window) and whose `clip_id` is the clip it produced.

**Highlights and markers.** Clipping every derived event is not the same as clipping every
interesting one: a 30-round Counter-Strike match is ~60 round transitions, and a clip for
each would fill the clips directory with footage nobody asked for. `EventKind::is_highlight`
is the single place that policy lives:

| clips (a highlight) | recorded only (a marker) |
|---|---|
| `Kill`, `Death`, `Assist`, `Objective`, `BombPlanted`, `BombDefused`, `GameEnd` | `GameStart`, `RoundStart`, `RoundEnd` |

A marker goes through `Recorder::note_event`, which writes an `events` row with `clip_id`
NULL and takes no footage. `GameEnd` clips on purpose (the last seconds of a match are what
people keep); `GameStart` would be a loading screen. This is a product judgement, it is one
`match` arm, and it is the first thing to revisit if the clips are not what you want.

**Consequence, stated plainly:** a burst of events (a triple kill, a bomb and a round end
inside one clip's post-roll) is clipped **sequentially** — one clip per highlight, in
arrival order, each with its own pre-roll. Events arriving during a clip wait in the channel.
There is no coalescing or de-bouncing yet.

**A store change this required.** `events.clip_id` is a foreign key, so the storage policy
would have failed with `FOREIGN KEY constraint failed` the first time it tried to evict a
clip that had an event. `Store::delete_clip_returning_path` now detaches the clip's events
(sets `clip_id` to NULL) in the same transaction as the delete: the rows that record *what
happened* stay, the footage goes. The delete-before-unlink ordering (spec §8.2) is unchanged.

---

## 4. What is verified on this machine

Every claim below is a test in the suite. Run
`CARGO_HOME="$PWD/target/cargo-home" cargo test --workspace --features localplay-encoder/test-encoders`
and, for the demonstration, `cargo run -p localplay-events --example gsi_demo`.

| claim | test |
|---|---|
| the relaxed client accepts a self-signed certificate, and a verifying client refuses the same one | `tests/lol_mock.rs::the_relaxed_client_accepts_the_self_signed_certificate_and_a_strict_one_does_not` (a real TLS server on loopback) |
| attaching to a game already in progress emits one event and then nothing | `lol_mock.rs::attaching_to_a_game_already_in_progress_does_not_replay_its_history` and `lol/derive.rs::attaching_to_a_game_already_in_progress_reports_the_attach_and_nothing_else` |
| a full history, an incremental event, and a second game are each derived once | `lol/derive.rs` (7 cases), `lol_mock.rs` (5 cases) |
| a malformed or `null` body is neither an event nor the end of a game | `lol_mock.rs::a_malformed_body_is_neither_an_event_nor_the_end_of_the_game`, `…::a_null_body_is_treated_as_the_game_being_over` |
| a refused connection is the normal no-game state | `lol/client.rs::a_refused_connection_is_the_no_game_case_and_not_a_failure` |
| the poller cannot be pointed anywhere but loopback | `lol/endpoint.rs` (6 cases), `lol_mock.rs::the_poller_only_ever_asks_for_loopback` |
| the GSI listener binds loopback and refuses anything else | `gsi.rs::a_listener_refuses_to_bind_anything_that_is_not_loopback`, `tests/gsi_listener.rs::the_listener_is_bound_to_loopback_and_answers_on_the_configured_path` |
| a correct token is accepted and derives the expected events | `gsi_listener.rs::a_post_with_the_configured_token_is_accepted_and_derives_the_expected_events` |
| a wrong, absent, short or long token is rejected and derives nothing | `gsi_listener.rs::a_post_with_a_wrong_token_is_rejected_and_derives_nothing`, `…::a_post_with_no_auth_block_at_all_is_rejected`, `gsi.rs::the_token_gate_accepts_only_the_exact_token` |
| a malformed body yields nothing and does not stop the listener | `gsi_listener.rs::a_malformed_body_yields_nothing_and_does_not_stop_the_listener` |
| the generated cfg names the port, the path and the token exactly once, and every line is a quoted pair | `gsi/integration.rs` (5 cases) |
| an event-triggered clip records its reason, linked to the clip; a marker records without a clip; a manual clip records nothing | `recorder/src/tests.rs::a_clip_records_why_it_was_taken` (a real recording, real ffmpeg, real SQLite) |
| evicting a clip detaches its events instead of failing | `store/src/lib.rs::deleting_a_clip_releases_its_events_instead_of_failing` |
| no HTTP client, no relaxed TLS and no outbound socket exists outside the loopback modules | `no_egress.rs` (4 tests) |

The test fixtures are canned payloads in `crates/events/tests/fixtures/` — see the README in
that directory, including why the committed TLS key is not a secret.

---

## 5. What is not verified

This is the honest part. **No League client and no CS2 or Dota 2 client was contacted while
writing any of this**, by requirement: the League client runs with kernel-level anti-cheat
and the machine it is installed on was not available, and there is no Windows host here at
all. Everything above was exercised against mock servers inside the test process, over
loopback, with canned payloads.

So, specifically:

1. **The payload shapes are documented, not observed.** The fixtures are written from the
   documented API and from public payload references. Every field is optional in the parser
   and no payload can panic it, but the *derivation* can only be as right as the shapes. If a
   real client sends something different (a renamed field, a different `EventName`), the
   symptom is missing events, not a failure.
2. **The League endpoint is never contacted, so the TLS handshake against Riot's actual
   certificate is unproven.** What is proven is that *a* self-signed certificate on
   `127.0.0.1` is accepted by the relaxed client and rejected by a verifying one.
3. **The GSI `auth` mechanism is unproven end to end.** Valve's documentation says the
   `"auth"` block's fields are transmitted as JSON string fields in the request body, and the
   listener reads `auth.token` because of that. No game was run to confirm it. If the token
   never matches in practice, this is the first thing to check — the listener logs the
   rejection (and never the token), so the symptom is a `403` and an empty `events` table.
4. **The generated cfg file has never been parsed by a game.** Its structure is asserted
   line by line (quoted pairs, the uri, one token, the subscribed blocks), which catches the
   mistakes a test can catch — it caught a half-quoted data block during development — but
   "CS2 accepts this file" is a claim only CS2 can make.
5. **Nothing has been run on Windows.** The crates cross-check for
   `x86_64-pc-windows-msvc`, which proves the integration code compiles for the target and
   exercises the Schannel path's types, but no socket has ever been opened there.
6. **Live behaviour is unmeasured**: the poll rate's effect on a real client, what a real
   event storm looks like (a 5-kill round), whether the highlights/markers split produces
   clips a player wants, and how the events table looks after a real session.
7. **Dota 2 observer mode** is handled defensively (an array `player` block parses, the first
   entry is used, no deltas are emitted across a `steamid` change) and has never been fed a
   real observer payload.

### Verifying it against a real game

For League: enable `lol_poll_enabled`, start a game (a bot game in the practice tool is
enough), and check that the log says a game was detected, that exactly one clip appears at
the attach, and that a kill produces one more. For CS2 or Dota: install the generated cfg
file into the game's `cfg` directory, restart the game, and play a round — the log should
show `game_start`, then a `kill`/`death`/`bomb_*` per moment, and the `events` table in
`<app data>\localplay.db` should contain one row per event, with `clip_id` set for the
highlights. Without any game, `cargo run -p localplay-events --example gsi_demo` plus a
`curl` of a fixture exercises the listener without touching a game process.

---

## 6. Dependencies added

Three, each with a reason (spec §3.1 requires a note for anything new):

| crate | why | alternative rejected |
|---|---|---|
| `native-tls` | the platform's TLS, so the LoL client's relaxed verification is a supported flag rather than a bundled stack; no C build step, so the Windows cross-check keeps working from macOS | `rustls` + `ring`: a C/asm build that cannot be cross-checked for `x86_64-pc-windows-msvc` from this host; an async HTTP client (`reqwest`, `tokio`) for one `GET` a second |
| `subtle` | constant-time comparison for the GSI token gate. Hand-rolling it is a bet that the optimiser keeps the loop branch-free | a hand-written XOR fold |
| `getrandom` | the token itself, from the OS, without a C build script | `SystemTime`-based entropy (guessable), a hand-written `BCryptGenRandom`/`getentropy` pair |

The HTTP framing for both directions is written in the crate (`wire.rs`, ~250 lines with its
tests) rather than pulled in: one `GET` and one `POST` do not need a general-purpose stack,
and the bounds that matter (head size, body size, chunked decoding) are the parts that are
exhaustively tested.
