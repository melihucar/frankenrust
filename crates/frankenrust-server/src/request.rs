//! Translates a hyper request into `frankenrust_core::context::Request`,
//! the port of `net/http.Request` fields upstream reads
//! (`vendor/frankenphp/context.go:23`).
use std::io::Cursor;
use std::net::SocketAddr;

use bytes::Bytes;
use hyper::http::request::Parts;
use hyper::Version;

use frankenrust_core::context::{Request, RequestBody};

/// Percent-decodes `%XX` escapes, byte for byte -- Go's `URL.Path`
/// (`net/url`'s `unescape` in "path" mode), which decodes `%XX` and leaves
/// every other byte, `+` included, untouched (`+` is only special in a
/// query string / form body, never in a path).
///
/// Deliberately not upstream's `URL.RequestURI()`/`EscapedPath()` round-trip
/// (see `context.rs`'s `Request::raw_target` doc comment): reproducing that
/// exactly -- re-escaping a decoded path when the wire form does not survive
/// a round trip -- is the server layer's job "tracked as its own issue", not
/// this one. A plain decode is correct for the common ASCII path this
/// issue's acceptance tests exercise; a wire path carrying raw UTF-8 or the
/// handful of bytes `encodePath` re-escapes will not byte-exactly match
/// upstream's `URL.Path` yet.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(input[i + 1]), hex_digit(input[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Builds a [`Request`] from a hyper request's parts, an already-collected
/// body, and the peer address the accept loop observed.
///
/// The body is fully buffered in memory before this is called (see
/// `server.rs`'s `handle`): this issue buffers both the request and
/// response bodies rather than streaming either, which is correct for the
/// [`RequestBody`]/[`frankenrust_core::context::ResponseSink`] contracts
/// (any blocking `Read + Send` source or `Mutex`-guarded sink satisfies
/// them) but not memory-efficient for a large upload or a large response,
/// and imposes no size bound of its own -- an unbounded request body is
/// exactly #147's concern (request header/body size limits), and #170 is
/// the response-side twin of this same buffering choice.
pub fn build_request(parts: &Parts, body: Bytes, remote_addr: SocketAddr) -> Request {
    let raw_target = parts
        .uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str().as_bytes().to_vec())
        .unwrap_or_else(|| b"/".to_vec());
    let path = percent_decode(parts.uri.path().as_bytes());
    let query = parts.uri.query().unwrap_or("").as_bytes().to_vec();
    let host = parts
        .headers
        .get(hyper::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (proto_major, proto_minor) = match parts.version {
        Version::HTTP_10 => (1, 0),
        Version::HTTP_09 => (0, 9),
        _ => (1, 1),
    };

    let mut request = Request::new(parts.method.as_str(), path)
        .with_raw_target(raw_target)
        .with_query(query)
        .with_content_length(body.len() as i64)
        .with_host(host)
        .with_remote_addr(remote_addr.to_string())
        .with_proto(proto_major, proto_minor);

    for (name, value) in &parts.headers {
        request
            .headers
            .insert(name.as_str(), value.as_bytes().to_vec());
    }

    // No genuine peer-closed signal is wired up here -- see this issue's
    // hazards note: "If your HTTP layer exposes a genuine peer-closed
    // signal you may simplify". hyper's low-level `http1::Connection`
    // future does resolve early on a client disconnect, but observing that
    // from inside a still-running `service_fn` call needs a second task
    // watching the connection and is not exercised by this issue's
    // acceptance tests, so `cancelled` is left at `Request::new`'s default
    // (never set) rather than wired to a half-built signal. `go_is_context_done`
    // and `client_has_closed()` therefore always read "still connected" in
    // this server; real disconnect detection is left for a follow-up, filed
    // as #146 (a background read watching the socket for EOF while the
    // handler is in flight, mirroring Go's `net/http` `backgroundRead`).
    request.body = RequestBody::new(Cursor::new(body.to_vec()));
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_leaves_plus_and_decodes_escapes() {
        assert_eq!(percent_decode(b"/a+b%20c"), b"/a+b c".to_vec());
    }

    #[test]
    fn percent_decode_leaves_a_malformed_escape_untouched() {
        assert_eq!(percent_decode(b"/100%"), b"/100%".to_vec());
        assert_eq!(percent_decode(b"/a%zzb"), b"/a%zzb".to_vec());
    }

    #[test]
    fn build_request_reads_method_path_and_headers() {
        let request: hyper::Request<()> = hyper::Request::builder()
            .method("GET")
            .uri("/index.php?x=1")
            .header("X-Test", "yes")
            .body(())
            .unwrap();
        let (parts, ()) = request.into_parts();
        let core_request = build_request(&parts, Bytes::new(), "127.0.0.1:9999".parse().unwrap());

        assert_eq!(core_request.method, "GET");
        assert_eq!(core_request.path, b"/index.php");
        assert_eq!(core_request.query, b"x=1");
        assert_eq!(
            core_request.headers.get_first("X-Test"),
            Some(b"yes".as_slice())
        );
        assert_eq!(core_request.remote_addr, "127.0.0.1:9999");
    }
}
