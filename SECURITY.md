# Security policy

## Reporting a vulnerability

Please report privately through GitHub: **Security → Report a vulnerability** on this
repository (private security advisories). Do not open a public issue for a vulnerability.
Expect an acknowledgement within a week.

## Scope and threat model

replayed is a local application: no accounts, no telemetry, no cloud. It listens only
on loopback (game-state-integration HTTP listener, token-gated; audio transport to ffmpeg) and
makes no outbound connections. In scope:

- a local attacker or malicious game config reaching the GSI listener,
- a malicious or corrupt clip / `replayed.db` causing file access outside the library
  (`delete_clip` refuses to unlink outside the clips and app-data directories),
- the webview (strict CSP, no filesystem/shell/http capability granted),
- supply chain: pinned, checksummed ffmpeg sidecars; `cargo-deny`, `npm audit` and gitleaks in CI.

Out of scope: an attacker who already runs code as the same user.
