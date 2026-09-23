//! The one place in localplay that relaxes TLS verification (spec §7.1).
//!
//! Riot's Live Client Data API is an HTTPS service on `127.0.0.1:2999` whose certificate is
//! self-signed, so there is nothing about it that *can* be verified: no name to match and
//! no authority to trust. The spec's constraint is therefore not "skip verification" but
//! "skip it only for a client that cannot talk to anything else", and that is what this
//! module is:
//!
//! * The relaxation is a private field of [`LoopbackClient`], built here and nowhere else.
//!   No public API hands out a connector, and none can build this type from a host name or
//!   a URL.
//! * The client's destination is an [`Endpoint`], which can only be built for a loopback
//!   address ([`Endpoint::loopback`] refuses anything else) — so the relaxed connector can
//!   only ever be pointed at this machine, whatever a caller does with it.
//! * `tests/no_egress.rs` asserts statically that `danger_accept_invalid_certs` appears in
//!   exactly one file in the tree, and that this is it.
//!
//! The request is written by hand: `GET <path> HTTP/1.1` with `Host: 127.0.0.1:2999` and
//! SNI disabled, since the certificate matches no name we could send. Reading the response
//! is [`crate::wire`]'s job, which is where the bounds and the tests for them live.

use crate::lol::endpoint::Endpoint;
use crate::wire::Framed;
use native_tls::{HandshakeError, TlsConnector};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long to wait for the TCP connection. Loopback either answers at once or not at all,
/// so a short budget is right: the poller runs once a second and must not be able to fall
/// behind by waiting a minute for a socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one read or write may take.
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// The largest response body accepted. `allgamedata` is a few hundred kilobytes for a
/// ten-player game; this is generous and still bounds what a hostile local process could
/// make the recorder allocate.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// What one poll of the endpoint found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The endpoint served a body (HTTP 200).
    Body(Vec<u8>),
    /// The endpoint is up and says there is no game to report (HTTP 404). This is the
    /// normal answer between games, and it is the signal that a game has ended.
    NotServing,
}

/// Why a poll could not produce a payload.
///
/// The variants are separated by what a caller does with them, not by what went wrong
/// underneath: [`FetchError::is_no_game`] is the "nothing is wrong, there is no game"
/// answer the poller must not log as an error every second.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The connection failed: refused (no game, or the game is not running), reset, or the
    /// interface went away. The ordinary case when nothing is playing.
    #[error("the connection to {0} failed: {1}")]
    Connection(SocketAddr, std::io::Error),
    /// The socket timed out. Loopback does not do this under normal load; a game that is
    /// loading its world can.
    #[error("the Live Client at {0} did not answer in time")]
    Timeout(SocketAddr),
    /// The TLS handshake failed — with a self-signed certificate accepted, this means the
    /// peer is not the Live Client (or is not speaking TLS at all).
    #[error("the TLS handshake with {0} failed: {1}")]
    Tls(SocketAddr, String),
    /// The peer answered, but not with HTTP this reader understands.
    #[error("the Live Client answered something malformed: {0}")]
    Protocol(String),
    /// The peer answered with a status localplay does not expect from this endpoint.
    #[error("the Live Client answered HTTP {0}")]
    UnexpectedStatus(u16),
}

impl FetchError {
    /// Whether this is the ordinary "no game is running" answer (spec §7.1: a connection
    /// refusal or a 404 between games must not be reported as a failure).
    pub fn is_no_game(&self) -> bool {
        matches!(self, FetchError::Connection(..) | FetchError::Timeout(..))
    }
}

/// The client that talks to the Live Client Data API — and to nothing else.
///
/// Its address is fixed at construction from an [`Endpoint`], so the relaxed verification
/// cannot be combined with an arbitrary host: the two are the same object.
pub struct LoopbackClient {
    endpoint: Endpoint,
    connector: TlsConnector,
}

impl LoopbackClient {
    /// The client for the real endpoint (spec §7.1). This is what production uses.
    pub fn live_client() -> Result<Self, FetchError> {
        Self::for_endpoint(Endpoint::live_client())
    }

    /// The same client for any loopback endpoint — how the tests reach a mock server.
    ///
    /// Takes the whole [`Endpoint`] rather than an address, so there is no overload that
    /// could be given something that is not loopback: the check happened in
    /// [`Endpoint::loopback`], and this constructor cannot undo it.
    pub fn for_endpoint(endpoint: Endpoint) -> Result<Self, FetchError> {
        debug_assert!(endpoint.is_loopback(), "an Endpoint is loopback by construction");
        let connector = TlsConnector::builder()
            // The two lines this module exists for. Both are scoped to a connector that
            // is stored in this struct and never handed out; see the module docs.
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            // No SNI: the Host header is an IP literal, and sending a name would only
            // involve a resolver in a loopback request.
            .use_sni(false)
            .build()
            .map_err(|e| FetchError::Tls(endpoint.addr(), format!("{e}")))?;
        Ok(Self { endpoint, connector })
    }

    /// The endpoint this client is pinned to.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// One `GET`. Returns the body, or [`Response::NotServing`] for a 404.
    ///
    /// A connection per call, deliberately: the poller asks once a second, a TLS handshake on
    /// loopback costs a few milliseconds, and holding a connection open to a game that may be
    /// exiting is a worse trade than paying for the handshake. Nothing here keeps state
    /// between polls — that is [`crate::lol::derive`]'s job, and it is pure.
    pub fn get(&self) -> Result<Response, FetchError> {
        let addr = self.endpoint.addr();
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| self.io_error(e))?;
        stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| self.io_error(e))?;
        stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| self.io_error(e))?;
        // The payload is small and the poller is synchronous: waiting for a delayed ACK
        // would be the slowest part of a one-second poll.
        let _ = stream.set_nodelay(true);

        let host = self.endpoint.host_header();
        let mut tls = match self.connector.connect(&host, stream) {
            Ok(tls) => tls,
            Err(HandshakeError::Failure(e)) => {
                return Err(FetchError::Tls(addr, format!("{e}")))
            }
            // A non-blocking handshake would be a bug here: the socket is blocking and
            // `connect` drives it to completion.
            Err(HandshakeError::WouldBlock(_)) => return Err(FetchError::Timeout(addr)),
        };

        // The request is assembled from the endpoint's own validated path, so the request
        // line cannot be broken by anything upstream (see `endpoint::is_valid_path`).
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\n\
             User-Agent: localplay/0.1\r\nConnection: close\r\n\r\n",
            self.endpoint.path()
        );
        tls.write_all(request.as_bytes()).map_err(|e| self.io_error(e))?;
        tls.flush().map_err(|e| self.io_error(e))?;

        let mut framed = Framed::new(tls);
        let head = framed.head().map_err(|e| FetchError::Protocol(format!("{e:#}")))?;
        let status = head
            .status()
            .ok_or_else(|| FetchError::Protocol("the response has no status line".into()))?;
        match status {
            200 => {
                let body = framed
                    .body(&head, MAX_RESPONSE_BYTES)
                    .map_err(|e| FetchError::Protocol(format!("{e:#}")))?;
                Ok(Response::Body(body))
            }
            404 => Ok(Response::NotServing),
            other => Err(FetchError::UnexpectedStatus(other)),
        }
    }

    fn io_error(&self, e: std::io::Error) -> FetchError {
        let addr = self.endpoint.addr();
        match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                FetchError::Timeout(addr)
            }
            _ => FetchError::Connection(addr, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_pinned_to_the_endpoint_it_was_given() {
        let addr: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep.clone()).expect("a relaxed connector");
        assert_eq!(client.endpoint(), &ep);
        assert!(client.endpoint().is_loopback());
    }

    #[test]
    fn the_live_client_constructs_and_is_on_loopback() {
        // Constructing it does not connect to anything: this test must pass on a machine
        // with no League client, which is every machine this project is built on.
        let client = LoopbackClient::live_client().expect("the relaxed connector builds");
        assert_eq!(client.endpoint().addr().port(), 2999);
        assert!(client.endpoint().is_loopback());
    }

    #[test]
    fn a_refused_connection_is_the_no_game_case_and_not_a_failure() {
        // Port 1 on loopback: nothing listens there, and this is exactly what polling
        // between games looks like.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep).unwrap();

        let err = client.get().expect_err("nothing is listening on port 1");
        assert!(err.is_no_game(), "{err} must read as 'no game is running'");
        assert!(
            matches!(err, FetchError::Connection(..)),
            "a refused loopback connection is a connection error, not a protocol one: {err}"
        );
    }

    #[test]
    fn an_endpoint_that_only_accepts_and_closes_is_a_protocol_error() {
        // A socket that accepts and immediately hangs up: the TLS handshake cannot
        // complete, which must be reported as such (and never as a panic).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep).unwrap();
        let err = client.get().expect_err("the peer is not a TLS server");
        assert!(
            matches!(err, FetchError::Tls(..)),
            "a peer that is not speaking TLS is a TLS error: {err}"
        );
        assert!(!err.is_no_game(), "and it is not silently read as 'no game'");
        let _ = server.join();
    }
}
