//! Spec §7.3: enforce "zero egress" by inspecting the source tree.
//!
//! These are static checks over the tree rather than runtime hopes, and they are the reason
//! the two integrations can be trusted with the constraints the spec puts on them:
//!
//! * no HTTP client is constructed anywhere except the League module, which can only address
//!   loopback ([`no_http_client_is_constructed_outside_the_loopback_module`]);
//! * TLS certificate verification is relaxed in exactly one file, and that file is the same
//!   one that pins the loopback address
//!   ([`certificate_verification_is_relaxed_in_exactly_one_place`]);
//! * every `TcpListener::bind` in the tree is pinned to loopback, and nothing in the tree
//!   binds every interface ([`a_listener_binds_loopback_only`]).
//!
//! A guard that passes for the wrong reason is worse than no guard, so each exemption below
//! is by path and carries its reason — and every check reads **code**, with `//` comment
//! lines removed first. Without that, this module's own subject matter ("the client that
//! relaxes verification", "never bind `0.0.0.0`") would satisfy the checks it is asserting,
//! which is precisely the failure mode these guards exist to avoid. (Only `//` line
//! comments are stripped; the tree uses no block comments, and one would show up as an
//! unexplained offender rather than as a silent pass.)

use std::path::Path;

/// The League module, which is the one place allowed to build its own client: it alone talks
/// to `127.0.0.1:2999` with a self-signed certificate (spec §7.1). Exempted as a module
/// rather than as a file, because it is split by layer — the client, the payload, the
/// derivation, the poller — and every one of those files is part of the same exempt module.
fn is_the_league_module(rel: &str) -> bool {
    rel == "crates/events/src/lol.rs" || rel.starts_with("crates/events/src/lol/")
}

/// The mock server the League tests drive, which builds a connector of its own **with
/// verification left on** — that is the assertion that the fixture certificate is untrusted,
/// so exempting it does not weaken anything: its connector cannot be relaxed by accident
/// without the `danger_accept_invalid_certs` guard below failing first.
fn is_the_league_mock(rel: &str) -> bool {
    rel == "crates/events/tests/lol_mock.rs"
}

#[test]
fn no_http_client_is_constructed_outside_the_loopback_module() {
    // The literals a client is built from. `TlsConnector::builder` was added when the LoL
    // client moved from a hypothetical `reqwest`/`ureq` to `native-tls`: the guard has to
    // name whatever this tree actually uses, or it would pass by not looking.
    let literals = ["reqwest::Client", "ureq::agent", "TlsConnector::builder"];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        if is_the_league_module(&rel) || is_the_league_mock(&rel) {
            continue;
        }
        // The guard must not scan itself: this file necessarily contains the very
        // literals it searches for. Exempted by path, explicitly, rather than by
        // splitting the literals.
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
fn certificate_verification_is_relaxed_in_exactly_one_place() {
    // The binding security constraint of spec §7.1, as a static property: the *set of files*
    // that turn verification off has to be exactly one, and it has to be the file that pins
    // the address. A second relaxed client anywhere in the tree — a general-purpose one,
    // which is the thing the constraint forbids — fails here rather than in review.
    const THE_ONE_FILE: &str = "crates/events/src/lol/client.rs";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        if rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("danger_accept_invalid_certs") {
            offenders.push(rel);
        }
    }

    assert_eq!(
        offenders,
        vec![THE_ONE_FILE.to_string()],
        "exactly one file may relax TLS verification, and it must be the loopback client"
    );

    // And that one file must be the one that pins the address: the relaxation and the
    // loopback endpoint are supposed to be the same object, not two decisions.
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
fn a_listener_binds_loopback_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for entry in walk_rs(&root) {
        let rel = entry.to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("TcpListener::bind") {
            // Any of the three is the same pin, spelled the way the code that has it
            // spells it: the literal address, `Ipv4Addr::LOCALHOST`, or a check of the
            // address's own `is_loopback` (which is what `gsi::bind` does before binding
            // anything). Comments are already stripped, so a mention in prose cannot
            // satisfy this.
            let pinned = ["127.0.0.1", "LOCALHOST", "is_loopback"]
                .iter()
                .any(|pin| text.contains(pin));
            assert!(
                pinned,
                "{} binds a listener without pinning loopback (expected one of 127.0.0.1, \
                 Ipv4Addr::LOCALHOST, is_loopback)",
                entry.display()
            );
            // A wildcard *bind* is the mistake worth catching — line by line, so that a
            // test asserting the refusal of `0.0.0.0` (which is what the listener's own
            // tests do) is not mistaken for the mistake itself.
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
    // The broader property the three checks above add up to: the tree's socket *clients* are
    // the League module and the test mock. A new `TcpStream::connect` elsewhere is a new
    // outbound path and belongs in this list deliberately, not by accident.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut connecting: Vec<String> = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
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
            "crates/events/src/lol/client.rs".to_string(),
            "crates/events/tests/lol_mock.rs".to_string(),
        ],
        "the tree's outbound sockets are the League client and the test that drives its \
         loopback mock; anything else needs a reason and an exemption here"
    );
}

/// A file's Rust code, with `//` comment lines removed. Doc comments are comments: what
/// these guards are about is what the code does, not what it says about itself.
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
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        // Prune build output and VCS metadata: `target/` holds the whole crates.io
        // registry cache, whose vendored sources would otherwise be walked (and
        // could trip the assertions) on every run.
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
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
