//! Spec §7.3: enforce "zero egress" by inspecting the source tree.
use std::path::Path;

#[test]
fn no_http_client_is_constructed_outside_the_loopback_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        // The LoL module is the one place allowed to build its own client,
        // because it alone talks to 127.0.0.1:2999 with a self-signed cert.
        if rel.ends_with("lol.rs") {
            continue;
        }
        // The guard must not scan itself: this file necessarily contains the very
        // literals it searches for. Exempted by path, explicitly, rather than by
        // splitting the literals — a guard that passes for the wrong reason is
        // worse than no guard.
        if rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        if text.contains("reqwest::Client") || text.contains("ureq::agent") {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "outbound HTTP client constructed outside the loopback module: {offenders:?}"
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
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        if text.contains("TcpListener::bind") {
            assert!(
                text.contains("127.0.0.1") || text.contains("LOOPBACK"),
                "{} binds a listener without pinning loopback",
                entry.display()
            );
        }
    }
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
