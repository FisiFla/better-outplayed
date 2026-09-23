# Test fixtures

Everything in this directory is data for the tests in `crates/events/tests/` and the unit
tests inside `crates/events/src/`. Nothing here is read by the application at run time.

## `lol_*.json` — Live Client Data API payloads

Canned `liveclientdata/allgamedata` documents, shaped from the documented API rather than
captured from a game: **no League client was contacted to produce or verify them**, by
requirement (the game runs with kernel-level anti-cheat, and the machine it is installed on
was not available). They are treated as untrusted input by the parser, and they exist so
that the derivation rules have a stable, reviewable input.

| file | what it is |
|---|---|
| `lol_in_progress.json` | a game well under way, with six events already in its history — the attach case |
| `lol_events_next.json` | the same game a minute later, two new events (a kill and a baron) |
| `lol_second_game.json` | a *different* game, with the clock restarted and event ids back at 0 |
| `lol_no_identity.json` | a document that names no player, but does report a champion kill |
| `lol_none.json` | the JSON literal `null` — the endpoint's "no game" body |
| `lol_empty.json` | `{}` — a client that is up with nothing to report |

## `gsi_*.json` — Valve Game State Integration bodies

Canned GSI POST bodies. Same caveat: shaped from Valve's documented blocks and from public
payload references, not captured from CS2 or Dota 2, neither of which was run.

| file | what it is |
|---|---|
| `gsi_cs2_competitive.json` | a Counter-Strike match mid-round, bomb planted, player scoreboard |
| `gsi_cs2_menu.json` | a client sitting in the main menu: no map, no game |
| `gsi_dota_in_game.json` | a Dota 2 match, scoreboard on the `player` block |
| `gsi_dota_observer.json` | a Dota 2 observer payload, where `player` is an **array** |

The `"auth"` block in these fixtures carries `"fixture-token-not-a-secret"`, which is
exactly what it says: the integration test starts its listener with that token so the
positive path can be exercised, and posts a *different* token for the rejection cases.

## `tls/` — a throwaway certificate for the mock HTTPS server

`cert.pem` and `key-pkcs8.pem` are a self-signed certificate for `127.0.0.1` (SAN
`IP:127.0.0.1`), generated with `openssl req -x509 -newkey rsa:2048 -days 365`, and the
matching PKCS#8 private key.

**This is not a secret and must not be treated as one.** It is a test fixture: it is
valid only for the loopback address, it is served only by the mock server inside
`tests/lol_mock.rs`, and it is committed on purpose so the suite needs no certificate
generation at run time. It exists to prove one property that matters — that the League
client's relaxed verification accepts a self-signed certificate *and that a client with
verification on rejects the very same certificate* — which cannot be shown without a
certificate that no real authority signed.

To regenerate:

```sh
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 365 -nodes \
    -subj "/CN=127.0.0.1/O=localplay test fixture" -addext "subjectAltName=IP:127.0.0.1"
openssl pkcs8 -topk8 -nocrypt -in key.pem -out key-pkcs8.pem && rm key.pem
```
