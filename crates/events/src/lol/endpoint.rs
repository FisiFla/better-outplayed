//! Where the Live Client Data API lives — and the type that cannot point anywhere else.
//!
//! Spec §7.1 fixes both the address and the path. This module exists so that neither is a
//! string a caller could substitute: an [`Endpoint`] can only be built for a **loopback**
//! address, so every layer above it (the relaxed-TLS client, the poller) inherits that
//! property and none of them can be aimed at another machine.
//!
//! Tests point a poller at a mock server with [`Endpoint::loopback`]. That is the same
//! constructor the live endpoint uses, with a different port — there is no "test mode" flag
//! and no environment variable, so a mock can never become the production default by
//! omission. The production default is [`Endpoint::live_client`], and its address is a
//! `const`.

use anyhow::{bail, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// The address Riot's game client serves the Live Client Data API on. Loopback, and always
/// port 2999.
pub const LIVE_CLIENT_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 2999);

/// The one path this module asks for: the whole game snapshot in one response.
pub const ALL_GAME_DATA_PATH: &str = "/liveclientdata/allgamedata";

/// The longest request path accepted, so a path can never grow a head past what
/// [`crate::wire`] will read.
pub const MAX_PATH_BYTES: usize = 512;

/// An HTTPS endpoint the League client can be reached at: a loopback socket plus a path.
///
/// The fields are private and the only constructor that is public checks the loopback
/// property, which is the whole point: `TcpStream::connect` is reached from
/// [`crate::lol::client`] through this type and never with a bare address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    addr: SocketAddr,
    path: String,
}

impl Endpoint {
    /// The real thing: `https://127.0.0.1:2999/liveclientdata/allgamedata` (spec §7.1).
    ///
    /// Built from the constants directly rather than through [`Endpoint::loopback`], so
    /// that this, the shipping default, cannot fail at run time. That the constants would
    /// also satisfy the validating constructor is asserted by a test.
    pub fn live_client() -> Self {
        Self { addr: LIVE_CLIENT_ADDR, path: ALL_GAME_DATA_PATH.to_string() }
    }

    /// An endpoint on a loopback address.
    ///
    /// Used by the tests to point a poller at a mock server, and by nothing else. It
    /// refuses any address that is not loopback — so a caller cannot reach the network, and
    /// an accidental `Endpoint::loopback(SocketAddr::from(([0, 0, 0, 0], 2999)), ..)` is an
    /// error rather than a listener on every interface.
    pub fn loopback(addr: SocketAddr, path: &str) -> Result<Self> {
        if !addr.ip().is_loopback() {
            bail!(
                "the League Live Client API is a loopback service, so an endpoint may only \
                 be built for a loopback address; {addr} is not one"
            );
        }
        if !is_valid_path(path) {
            bail!(
                "a request path must start with '/' and contain no whitespace or control \
                 characters, and be at most {MAX_PATH_BYTES} bytes: {path:?}"
            );
        }
        Ok(Self { addr, path: path.to_string() })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The loopback property the rest of the module relies on.
    pub fn is_loopback(&self) -> bool {
        self.addr.ip().is_loopback()
    }

    /// The `Host:` header value — the address, never a name: the certificate being
    /// self-signed means there is no name to match, and sending one would only invite a
    /// resolver to be involved.
    pub fn host_header(&self) -> String {
        self.addr.to_string()
    }

    /// The URL as a human would write it. For logs and error messages only; the client
    /// never parses a URL (that is the point of this type).
    pub fn url(&self) -> String {
        format!("https://{}{}", self.addr, self.path)
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::live_client()
    }
}

/// A path is written into a request line, so anything that could end it early, start a new
/// header, or smuggle a control character is refused rather than escaped. Paths here are
/// compile-time constants or test literals; nothing a user can configure reaches this.
fn is_valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= MAX_PATH_BYTES
        && !path.bytes().any(|b| b <= 0x20 || b == 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_live_client_endpoint_is_what_the_spec_names() {
        let ep = Endpoint::live_client();
        assert_eq!(ep.addr(), LIVE_CLIENT_ADDR);
        assert_eq!(ep.addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(ep.addr().port(), 2999);
        assert_eq!(ep.path(), "/liveclientdata/allgamedata");
        assert_eq!(ep.url(), "https://127.0.0.1:2999/liveclientdata/allgamedata");
        assert!(ep.is_loopback());
        assert_eq!(ep.host_header(), "127.0.0.1:2999");
        assert_eq!(Endpoint::default(), ep, "the default is the real endpoint");
    }

    #[test]
    fn the_live_client_endpoint_would_pass_the_validating_constructor() {
        // The production default is built field-by-field so that it cannot fail at run
        // time; this states that the construction it skips would have accepted it.
        assert_eq!(
            Endpoint::loopback(LIVE_CLIENT_ADDR, ALL_GAME_DATA_PATH).expect("valid"),
            Endpoint::live_client()
        );
    }

    #[test]
    fn an_endpoint_cannot_be_built_for_a_non_loopback_address() {
        for addr in [
            "0.0.0.0:2999",       // every interface
            "192.168.1.10:2999",  // the LAN
            "10.0.0.1:2999",      // a private range
            "8.8.8.8:2999",       // the internet
            "[::]:2999",          // every interface, v6
            "[fe80::1]:2999",     // link-local v6
        ] {
            let addr: SocketAddr = addr.parse().expect("a socket address");
            let err = Endpoint::loopback(addr, ALL_GAME_DATA_PATH)
                .expect_err("only loopback is allowed");
            assert!(
                err.to_string().contains("loopback"),
                "the error must say why: {err}"
            );
        }
    }

    #[test]
    fn a_loopback_v6_address_is_allowed() {
        let addr: SocketAddr = "[::1]:2999".parse().unwrap();
        assert!(Endpoint::loopback(addr, ALL_GAME_DATA_PATH).is_ok());
    }

    #[test]
    fn a_mock_endpoint_is_a_loopback_address_and_an_ordinary_path() {
        let addr: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        assert_eq!(ep.addr().port(), 51234);
        assert!(ep.is_loopback());
    }

    #[test]
    fn a_path_that_could_break_the_request_line_is_refused() {
        let addr: SocketAddr = "127.0.0.1:2999".parse().unwrap();
        for path in [
            "",                              // not a path
            "liveclientdata",                // no leading slash
            "/x HTTP/1.1\r\nHost: evil",     // header injection
            "/x\nHost: evil",                // line break
            "/x\t",                          // whitespace
            "/x\u{7f}",                      // a control character
        ] {
            assert!(
                Endpoint::loopback(addr, path).is_err(),
                "{path:?} must not become a request line"
            );
        }
        assert!(
            Endpoint::loopback(addr, &format!("/{}", "x".repeat(MAX_PATH_BYTES))).is_err(),
            "an over-long path is refused"
        );
    }
}
