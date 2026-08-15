//! Acceptance test 1 (issue #13): starts the server on an ephemeral port,
//! serves `bench/apps/hello/index.php`, and asserts the body and
//! `Content-Type` -- through `http::HeaderMap`, whose lookup is
//! case-insensitive by construction, matching this issue's "assert
//! `Content-Type` through an HTTP client that matches header names
//! case-insensitively" (byte-exact header casing is #159's).
//!
//! `init_php_threads` (`frankenrust_core::thread`) can only succeed once per
//! process, so this is the only `#[tokio::test]` in this binary -- Cargo
//! compiles every `tests/*.rs` file as its own process, which is what keeps
//! this test and `tests/concurrency.rs` from colliding on that constraint.
mod support;

use std::collections::HashMap;

use frankenrust_server::ServerConfig;

#[tokio::test]
async fn one_request_end_to_end_serves_hello_world() {
    let listener = frankenrust_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("binding an ephemeral port must succeed");
    let addr = listener
        .local_addr()
        .expect("a bound listener has a local address");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let config = ServerConfig {
        document_root: support::hello_document_root(),
        num_threads: 1,
        php_ini: HashMap::new(),
    };

    let server = tokio::spawn(frankenrust_server::run(listener, config, async {
        let _ = shutdown_rx.await;
    }));

    let (status, headers, body) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        support::get(addr, "/index.php"),
    )
    .await
    .expect("the request must complete well within 30s");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), server)
        .await
        .expect("server must shut down within 30s -- a hang here is a lost wakeup in the dispatch/drain path")
        .expect("the server task must not panic")
        .expect("run() must return Ok on a clean shutdown");

    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), b"Hello World");

    let content_type = headers.get("content-type").expect(
        "PHP must have sent a Content-Type header for a script with no explicit header() call",
    );
    assert!(
        content_type
            .to_str()
            .expect("a default Content-Type value must be valid ASCII")
            .to_ascii_lowercase()
            .starts_with("text/html"),
        "expected PHP's default text/html Content-Type, got {content_type:?}"
    );
}
