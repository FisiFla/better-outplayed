//! The updater's trust anchor, and the shape of the check it makes.
//!
//! The updater is the one thing in this tree that reaches another machine, so its trust
//! anchor is worth pinning by a test rather than by a promise:
//!
//! - **the key.** `plugins.updater.pubkey` in `tauri.conf.json` is the minisign public key
//!   every downloaded artifact is checked against. It has to be a key the plugin can parse,
//!   because a key it cannot parse is a key it refuses everything with — and the failure
//!   would only show up on a user's machine, at the moment they wanted an update.
//! - **the endpoint.** There is exactly one, and it is over HTTPS. The frontend cannot
//!   change it and the webview cannot open a connection of its own.
//! - **the check.** The plugin verifies the artifact's signature before it installs it.
//!   That check is unit-testable, and this file drives it: a good artifact verifies, a
//!   tampered one does not, and a signature made by a different key does not either.
//!
//! **What this file does not prove.** It does not prove that a real release was signed and
//! installed: the private key is not in this repository (it is a CI secret), so no test
//! here can sign anything the pinned key would accept. The check is exercised with a
//! throwaway key, and the pinned key is checked for shape and for being *not* that
//! throwaway. `docs/verification-status.md` §27 records the same boundary.

#![allow(clippy::unwrap_used)]

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use std::path::Path;

/// The configuration file the plugin reads its settings from.
const CONFIG: &str = "tauri.conf.json";

/// The one URL the updater may ask.
///
/// GitHub serves the newest release's asset at this stable path, which is why the release
/// workflow publishes `latest.json` as an asset of every release.
const ENDPOINT: &str = "https://github.com/FisiFla/replayed/releases/latest/download/latest.json";

/// A throwaway key pair, and a signature it made.
///
/// The private key behind this is not in this repository and never was: it was made for
/// this test, signed one file, and thrown away. The public key and the signature are here
/// because a test needs a *fixed* vector to check against.
const TEST_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQ2NDYzMEM0QUQxQjQyMTMKUldRVFFodXR4REJHMXQrMmtWMzE0Y2hPNWJOYXFNR2Y4L1M5K0JQdWR0MFczeHV1Z0VyUW5FaTQK";

/// The signature the throwaway key made over [`TEST_ARTIFACT`].
const TEST_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRVFFodXR4REJHMXJvd295b3JKUFBRb1dRNzZ6UXVCKzFpUGtKUHRxWGszRkJwa3hPdnNkR1J3VkxxdTVNNVVsVVFZK3RLVkhEZ3ZlR2cvTEd4L0FVMzRxNWFtK3VNdEFZPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyNDg0NDAyCWZpbGU6YXJ0aWZhY3QuYmluCXZlcnNpb246OS45LjkKMDBNOFJqR045N01heDNXUjNDVEFITEg1TmRkVGZZUmxySGFuVEZGV1dHNWxJQjlsVUZVZWU3SXpXRDB5T3VmaDhFZEZsYUU4RWlnRlBTQ0NSNW95Q2c9PQo=";

/// The file the throwaway key signed.
const TEST_ARTIFACT: &[u8] =
    b"replayed updater test vector: a payload the updater would have signed.\n";

/// The version the throwaway signature was bound to.
///
/// The Tauri CLI records it in the signature's *trusted* comment, which the signature
/// itself covers — which is what stops a tampered manifest from announcing a version the
/// artifact was not signed for.
const TEST_SIGNED_VERSION: &str = "9.9.9";

/// The configuration, as the plugin reads it.
fn config() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CONFIG);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} could not be read: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not valid JSON: {err}", path.display()))
}

/// The pinned public key, as the configuration spells it.
fn pinned_key() -> String {
    config()["plugins"]["updater"]["pubkey"]
        .as_str()
        .unwrap_or("plugins.updater.pubkey is missing or is not a string")
        .to_string()
}

/// A minisign public key, decoded the way the plugin decodes it.
fn decode_key(encoded: &str) -> PublicKey {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .expect("the key is not valid base64");
    let text = String::from_utf8(raw).expect("the key is not UTF-8");
    PublicKey::decode(&text).expect("the key is not a minisign public key")
}

/// A minisign signature, decoded the way the plugin decodes it.
fn decode_signature(encoded: &str) -> Signature {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .expect("the signature is not valid base64");
    let text = String::from_utf8(raw).expect("the signature is not UTF-8");
    Signature::decode(&text).expect("the signature is not a minisign signature")
}

#[test]
fn the_updater_is_configured_to_produce_artifacts_and_to_pin_where_they_come_from() {
    let config = config();

    assert_eq!(
        config["bundle"]["createUpdaterArtifacts"],
        serde_json::json!(true),
        "the bundle must produce updater artifacts, or there is nothing for the updater \
         to install"
    );

    let endpoints = config["plugins"]["updater"]["endpoints"]
        .as_array()
        .expect("plugins.updater.endpoints must be a list");
    assert_eq!(
        endpoints.len(),
        1,
        "there is exactly one endpoint: the updater asks one URL or it does not ask"
    );
    assert_eq!(endpoints[0], serde_json::json!(ENDPOINT));
    assert!(
        ENDPOINT.starts_with("https://"),
        "the endpoint must be over TLS: {ENDPOINT}"
    );
    assert!(
        ENDPOINT.contains("github.com/FisiFla/replayed/"),
        "the endpoint must be this project's own release page and nothing else: {ENDPOINT}"
    );
}

#[test]
fn the_manifest_is_bound_to_the_version_it_was_signed_for() {
    // `requireSignedVersion` is what stops a tampered manifest from announcing a new
    // version number next to an older artifact's URL and signature — a downgrade, with a
    // validly signed file. It only works if the signature carries the version, so the
    // setting and the CLI's signing have to agree; this is the half that can be checked
    // without the release key.
    assert_eq!(
        config()["plugins"]["updater"]["requireSignedVersion"],
        serde_json::json!(true),
        "the signed version must be required, or the manifest can pair a version number \
         with an artifact that was not signed for it"
    );

    let signature = decode_signature(TEST_SIGNATURE);
    let comment = signature.trusted_comment();
    assert!(
        comment.contains(&format!("version:{TEST_SIGNED_VERSION}")),
        "the signature's trusted comment must carry the version it was signed for, so the \
         plugin can compare it with the one the manifest announced; it carried {comment:?}"
    );
}

#[test]
fn the_pinned_key_is_a_key_the_plugin_can_use() {
    let key = pinned_key();
    assert!(!key.is_empty(), "the updater must pin a public key");
    // A key the plugin cannot parse is a key it refuses every artifact with, and that
    // would only be discovered on a user's machine, at the moment they wanted an update.
    decode_key(&key);
}

#[test]
fn the_plugin_accepts_the_configuration_it_is_given() {
    // The plugin parses `plugins.updater` at startup, and a configuration it cannot parse
    // is a shell that does not start. This is the plugin's own type, so the shape is
    // checked by the thing that has to read it — and the endpoint's scheme is checked by
    // the plugin's own validation, which refuses anything that is not HTTPS unless the
    // dangerous flag is set.
    let updater = config()["plugins"]["updater"].clone();
    let parsed: tauri_plugin_updater::Config =
        serde_json::from_value(updater).expect("the plugin must be able to parse plugins.updater");
    assert_eq!(parsed.endpoints.len(), 1);
    assert!(parsed.require_signed_version);
    assert_eq!(parsed.pubkey, pinned_key());
}

#[test]
fn the_pinned_key_is_not_the_throwaway_one_this_test_uses() {
    // The test vector is a throwaway key pair. If it were ever the pinned key, the test
    // below would be checking the check against itself.
    assert_ne!(
        pinned_key().trim(),
        TEST_PUBLIC_KEY.trim(),
        "the pinned key must not be the throwaway key the test vector was signed with"
    );

    // And the check says so in the only way that matters: the pinned key does not accept
    // a signature the throwaway key made.
    let pinned = decode_key(&pinned_key());
    let signature = decode_signature(TEST_SIGNATURE);
    assert!(
        pinned.verify(TEST_ARTIFACT, &signature, true).is_err(),
        "the pinned key must not accept a signature made by another key"
    );
}

#[test]
fn the_signature_check_accepts_a_good_artifact_and_refuses_a_tampered_one() {
    let key = decode_key(TEST_PUBLIC_KEY);
    let signature = decode_signature(TEST_SIGNATURE);

    assert!(
        key.verify(TEST_ARTIFACT, &signature, true).is_ok(),
        "the artifact the key signed must verify"
    );

    // One byte changed, one byte too many: the artifact is no longer the one that was
    // signed, and the check has to say so.
    let tampered: Vec<u8> = TEST_ARTIFACT
        .iter()
        .copied()
        .map(|byte| byte.wrapping_add(1))
        .collect();
    assert!(
        key.verify(&tampered, &signature, true).is_err(),
        "a tampered artifact must not verify"
    );

    // The other half of a swap: a *different* file, signed by the right key, offered in
    // place of the one the manifest named. The signature is over the other file, so the
    // check must refuse it.
    let other = b"a different artifact entirely\n";
    assert!(
        key.verify(other, &signature, true).is_err(),
        "an artifact that was not signed must not verify"
    );
}
