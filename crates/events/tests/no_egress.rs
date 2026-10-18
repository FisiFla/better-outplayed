//! Zero egress, enforced by inspecting the source tree.
//!
//! Static checks rather than runtime hopes:
//!
//! * no HTTP client is constructed outside the League module, which can only address loopback
//!   ([`no_http_client_is_constructed_outside_the_loopback_module`]);
//! * TLS certificate verification is relaxed in exactly one file, the one that pins the
//!   loopback address ([`certificate_verification_is_relaxed_in_exactly_one_place`]);
//! * every `TcpListener::bind` is pinned to loopback and nothing binds every interface
//!   ([`the_gsi_listener_binds_loopback_only`]).
//!
//! Each exemption is by path and carries its reason. Every check reads code with `//` comment
//! lines removed first, otherwise this module's own prose ("never bind `0.0.0.0`") would
//! satisfy the checks it asserts. Block comments are not stripped; the tree uses none, so one
//! would show up as an offender rather than a silent pass.

use std::path::Path;

/// The League module, the one place allowed to build its own client: it alone talks to
/// `127.0.0.1:2999` with a self-signed certificate. Exempted as a module because it is split
/// by layer (client, payload, derivation, poller) and every file belongs to it.
fn is_the_league_module(rel: &str) -> bool {
    rel == "crates/events/src/lol.rs" || rel.starts_with("crates/events/src/lol/")
}

/// The mock server the League tests drive. It builds a connector with verification left on,
/// which asserts the generated CA is untrusted, so exempting it weakens nothing: it cannot be
/// relaxed without the `danger_accept_invalid_certs` guard failing first.
fn is_the_league_mock(rel: &str) -> bool {
    rel == "crates/events/tests/lol_mock.rs"
}

#[test]
fn no_http_client_is_constructed_outside_the_loopback_module() {
    // The literals a client is built from. The guard must name whatever this tree actually uses
    // (`native-tls`), or it passes by not looking.
    let literals = ["reqwest::Client", "ureq::agent", "TlsConnector::builder"];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        if is_the_league_module(&rel) || is_the_league_mock(&rel) {
            continue;
        }
        // The guard must not scan itself: this file contains the literals it searches for.
        if rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if literals.iter().any(|literal| text.contains(literal)) {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "an HTTP client is constructed outside the loopback module ({}): {offenders:?}",
        literals.join(", ")
    );
}

#[test]
fn certificate_verification_is_pinned_and_only_the_hostname_check_is_relaxed() {
    // The League client trusts Riot's pinned root only, so no file may turn chain verification
    // off, and the one remaining relaxation (the host-name match, which an IP literal cannot
    // satisfy) lives in exactly one file: the one that pins the address.
    const THE_ONE_FILE: &str = "crates/events/src/lol/client.rs";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut chain_offenders = Vec::new();
    let mut name_offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        if rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("danger_accept_invalid_certs") {
            chain_offenders.push(rel.clone());
        }
        if text.contains("danger_accept_invalid_hostnames") {
            name_offenders.push(rel);
        }
    }

    assert!(
        chain_offenders.is_empty(),
        "nothing may skip certificate-chain verification; pin a root instead: {chain_offenders:?}"
    );
    assert_eq!(
        name_offenders,
        vec![THE_ONE_FILE.to_string()],
        "exactly one file may relax the host-name check, and it must be the loopback client"
    );

    // The client must really pin: platform roots off, Riot's root added.
    let client = code_of(&root.join(THE_ONE_FILE));
    assert!(
        client.contains("disable_built_in_roots(true)"),
        "platform roots must be disabled"
    );
    assert!(
        client.contains("add_root_certificate"),
        "the pinned root must be added"
    );

    // That file must also pin the address: the relaxation and the loopback endpoint are one
    // decision, not two.
    let client = code_of(&root.join(THE_ONE_FILE));
    assert!(
        client.contains("is_loopback"),
        "the file that relaxes verification must also be the file that checks loopback"
    );
    assert!(
        client.contains("Endpoint"),
        "and must address an Endpoint, which cannot be built for a non-loopback host"
    );
}

#[test]
fn the_gsi_listener_binds_loopback_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for entry in walk_rs(&root) {
        let rel = entry.to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("TcpListener::bind") {
            // Any of the three is the same pin, however the code spells it: the literal address,
            // `Ipv4Addr::LOCALHOST`, or an `is_loopback` check (as `gsi::bind` does). Comments are
            // stripped, so prose cannot satisfy this.
            let pinned = ["127.0.0.1", "LOCALHOST", "is_loopback"]
                .iter()
                .any(|pin| text.contains(pin));
            assert!(
                pinned,
                "{} binds a listener without pinning loopback (expected one of 127.0.0.1, \
                 Ipv4Addr::LOCALHOST, is_loopback)",
                entry.display()
            );
            // A wildcard bind is the mistake to catch, line by line, so a test asserting the refusal of
            // `0.0.0.0` is not mistaken for it.
            let wildcard: Vec<&str> = text
                .lines()
                .filter(|line| line.contains("TcpListener::bind") && line.contains("0.0.0.0"))
                .collect();
            assert!(
                wildcard.is_empty(),
                "{} binds every interface: {wildcard:?}",
                entry.display()
            );
        }
    }
}

#[test]
fn only_the_loopback_modules_talk_to_a_socket_at_all() {
    // The socket clients are the League module and the test mock. A new `TcpStream::connect`
    // elsewhere is a new outbound path and belongs in this list deliberately.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut connecting: Vec<String> = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("TcpStream::connect") {
            connecting.push(rel);
        }
    }

    connecting.sort();
    assert_eq!(
        connecting,
        vec![
            // The listener's own unit tests dial its loopback socket (rate-limiter test).
            "crates/events/src/gsi.rs".to_string(),
            "crates/events/src/lol/client.rs".to_string(),
            "crates/events/tests/gsi_listener.rs".to_string(),
            "crates/events/tests/lol_mock.rs".to_string(),
        ],
        "the tree's outbound sockets are the League client and the tests that drive the \
         loopback mocks; anything else needs a reason and an exemption here"
    );
}

/// A file's Rust code with `//` comment lines removed (doc comments included): the guards are
/// about what the code does, not what it says about itself.
fn code_of(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk_rs(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        // Prune build output and VCS metadata: `target/` holds the crates.io registry cache, whose
        // vendored sources would otherwise be walked on every run.
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if p.is_dir() && (name == "target" || name == ".git") {
            continue;
        }
        if p.is_dir() {
            out.extend(walk_rs(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
