//! What the GSI listener accepts on the wire, and what it answers with.
//!
//! The listener is a server on a socket any process on this machine can reach, so this
//! module is written as a gate rather than as a convenience: the request must be a `POST`
//! to the configured path, it must declare its body length, and the body is bounded before
//! a single byte of it is read. Everything that fails is answered with a status and a
//! closed connection — nothing is logged here at all, because at this point in the request
//! every byte is attacker-controlled, and the logging decision belongs to
//! [`crate::gsi`], which knows what it is allowed to say.

use crate::wire::{Framed, Head};
use std::io::Read;

/// The largest body accepted. A CS2 payload with `allplayers` is a few hundred kilobytes;
/// Dota 2's is smaller. Two megabytes is generous for a loopback POST from a game client
/// and still bounds what a local process can make the recorder parse.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// A request that got as far as the token gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub body: Vec<u8>,
}

/// Why a request was refused before it could be looked at.
///
/// Each variant carries the status the connection is answered with. `MethodNotAllowed` and
/// `NotFound` are 405/404 because a wrong method or path is a client mistake; the rest are
/// refusals of a body localplay will not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Not a `POST`.
    MethodNotAllowed,
    /// A path other than the configured one.
    NotFound,
    /// No `Content-Length`, or a chunked body. GSI sends a length; refusing anything else
    /// keeps the listener from having to guess when a request ends.
    LengthRequired,
    /// A body over [`MAX_BODY_BYTES`], or one that is shorter than it claims.
    TooLarge,
    /// Not HTTP, or not something this reader understands.
    Malformed,
}

impl Reject {
    pub fn status(self) -> u16 {
        match self {
            Reject::MethodNotAllowed => 405,
            Reject::NotFound => 404,
            Reject::LengthRequired => 411,
            Reject::TooLarge => 413,
            Reject::Malformed => 400,
        }
    }
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Reject::MethodNotAllowed => "the method is not POST",
            Reject::NotFound => "the path is not the configured one",
            Reject::LengthRequired => "the body has no declared length",
            Reject::TooLarge => "the body is over the limit or shorter than it claims",
            Reject::Malformed => "the request is not HTTP this listener understands",
        };
        f.write_str(text)
    }
}

/// Read one request from `stream`, which must be `POST <path>` with a declared body length.
pub fn read_request(stream: &mut impl Read, path: &str) -> Result<Request, Reject> {
    let mut framed = Framed::new(stream);
    let head = framed.head().map_err(|_| Reject::Malformed)?;

    if head.method() != Some("POST") {
        return Err(Reject::MethodNotAllowed);
    }
    if !target_matches(&head, path) {
        return Err(Reject::NotFound);
    }
    if head.chunked() {
        return Err(Reject::LengthRequired);
    }
    let Some(length) = head.content_length() else {
        return Err(Reject::LengthRequired);
    };
    if length > MAX_BODY_BYTES as u64 {
        return Err(Reject::TooLarge);
    }
    let body = framed.body(&head, MAX_BODY_BYTES).map_err(|_| Reject::TooLarge)?;
    Ok(Request { body })
}

/// Whether the request target is the configured path. A query string is ignored: the path
/// is what routes, and nothing in a query is ever read.
fn target_matches(head: &Head, path: &str) -> bool {
    match head.target() {
        Some(target) => target.split('?').next() == Some(path),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read(raw: &[u8]) -> Result<Request, Reject> {
        read_request(&mut Cursor::new(raw.to_vec()), "/gsi")
    }

    #[test]
    fn reads_a_post_to_the_configured_path() {
        let raw = b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1:45671\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let request = read(raw).expect("a well-formed GSI POST");
        assert_eq!(request.body, b"{\"a\":1}");
    }

    #[test]
    fn a_query_string_does_not_hide_the_path() {
        let raw = b"POST /gsi?token=x HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        assert!(read(raw).is_ok(), "the query is ignored, not part of the route");
    }

    #[test]
    fn a_method_other_than_post_is_405() {
        for method in ["GET", "PUT", "DELETE", "HEAD", "post"] {
            // The reader upper-cases the method, so a lower-case `post` is also a POST.
            let raw = format!("{method} /gsi HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
            let result = read(raw.as_bytes());
            if method == "post" {
                assert_eq!(result, Ok(Request { body: b"{}".to_vec() }), "the method is case-insensitive");
            } else {
                assert_eq!(result, Err(Reject::MethodNotAllowed), "{method}");
            }
        }
    }

    #[test]
    fn another_path_is_404() {
        for target in ["/", "/liveclientdata/allgamedata", "/gsi/", "/gsix", ""] {
            let raw = format!("POST {target} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
            assert_eq!(read(raw.as_bytes()), Err(Reject::NotFound), "{target:?}");
        }
    }

    #[test]
    fn a_body_without_a_declared_length_is_411() {
        let raw = b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n{\"a\":1}";
        assert_eq!(read(raw), Err(Reject::LengthRequired));
    }

    #[test]
    fn a_chunked_body_is_refused_rather_than_guessed_at() {
        let raw = b"POST /gsi HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n";
        assert_eq!(read(raw), Err(Reject::LengthRequired), "GSI sends a length");
    }

    #[test]
    fn a_body_over_the_limit_is_413_before_it_is_read() {
        let raw = format!(
            "POST /gsi HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(read(raw.as_bytes()), Err(Reject::TooLarge));
    }

    #[test]
    fn a_body_shorter_than_it_claims_is_413_and_not_a_partial_read() {
        let raw = b"POST /gsi HTTP/1.1\r\nContent-Length: 500\r\n\r\n{\"a\":1}";
        assert_eq!(read(raw), Err(Reject::TooLarge));
    }

    #[test]
    fn a_request_that_is_not_http_is_400() {
        assert_eq!(read(b"garbage\r\n\r\n"), Err(Reject::Malformed));
        assert_eq!(read(b""), Err(Reject::Malformed));
        assert_eq!(read(b"POST /gsi\r\n\r\n"), Err(Reject::Malformed), "no HTTP version");
    }

    #[test]
    fn an_over_long_head_is_400() {
        let mut raw = b"POST /gsi HTTP/1.1\r\n".to_vec();
        raw.extend(vec![b'X'; 64 * 1024]);
        assert_eq!(read(&raw), Err(Reject::Malformed));
    }

    #[test]
    fn the_statuses_the_gate_answers_with_are_the_ones_it_documents() {
        assert_eq!(Reject::Malformed.status(), 400);
        assert_eq!(Reject::NotFound.status(), 404);
        assert_eq!(Reject::MethodNotAllowed.status(), 405);
        assert_eq!(Reject::LengthRequired.status(), 411);
        assert_eq!(Reject::TooLarge.status(), 413);
        for reject in [
            Reject::Malformed,
            Reject::NotFound,
            Reject::MethodNotAllowed,
            Reject::LengthRequired,
            Reject::TooLarge,
        ] {
            // Every refusal explains itself, because a user debugging an installation sees
            // these in the log.
            assert!(!reject.to_string().is_empty());
        }
    }
}
