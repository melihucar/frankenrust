//! Shared test support: a minimal HTTP/1.1 client built on the same
//! low-level `hyper::client::conn::http1` API the server side uses for
//! `hyper::server::conn::http1`, plus the shared `bench/apps/hello` document
//! root every acceptance test in this crate serves.
//!
//! Not a `tests/*.rs` file of its own -- `tests/support/mod.rs` is Cargo's
//! convention for a module shared by other integration test binaries
//! (`tests/foo.rs`, `tests/bar.rs`) without becoming a test binary itself.
use std::net::SocketAddr;
use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::http::{HeaderMap, HeaderValue, StatusCode};
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

/// `bench/apps/hello`, resolved from this crate's manifest directory so the
/// test does not depend on `cargo test`'s working directory.
pub fn hello_document_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/apps/hello")
}

/// Issues one GET request over a fresh connection and returns the response's
/// status, headers (case-insensitive lookup, same as any `http::HeaderMap`),
/// and fully-collected body.
pub async fn get(addr: SocketAddr, path: &str) -> (StatusCode, HeaderMap<HeaderValue>, Bytes) {
    let stream = TcpStream::connect(addr)
        .await
        .expect("test client must be able to connect to the server under test");
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("HTTP/1.1 client handshake must succeed against our own server");

    tokio::spawn(async move {
        if let Err(err) = connection.await {
            eprintln!("frankenrust test client: connection error: {err}");
        }
    });

    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("Host", "localhost")
        .body(Empty::<Bytes>::new())
        .expect("a bodiless GET request is always constructible");

    let response = sender
        .send_request(request)
        .await
        .expect("send_request must succeed against a live server");

    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body must be fully readable")
        .to_bytes();

    (status, headers, body)
}
