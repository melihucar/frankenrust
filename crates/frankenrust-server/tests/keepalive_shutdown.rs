//! Regression test for the shutdown half of acceptance criterion 2 ("no
//! deadlock, and a clean shutdown afterwards").
//!
//! `tests/hello_world.rs` and `tests/concurrency.rs` both assert a clean
//! shutdown, but neither could ever have caught the defect below:
//! `support::get` drops its `SendRequest` when it returns, which closes the
//! client connection, so both of those tests reach `shutdown_tx.send(())` with
//! zero open connections. That is the *only* case in which a
//! `serve_connection` future resolves on its own -- it waits for the peer to
//! hang up, and HTTP/1.1 keep-alive means the peer normally does not. Without
//! `hyper::server::conn::http1::Connection::graceful_shutdown()`, one idle-but-
//! open connection -- a browser tab, a pooled client, a reverse proxy between
//! requests -- pins `run`'s join loop forever and `drain_php_threads()` is
//! never reached: the PHP pool stays live and TSRM is never torn down.
//!
//! So this test does the one thing the other two structurally cannot: it holds
//! the connection open across the shutdown.
//!
//! One `#[tokio::test]` per binary, for the same reason `tests/hello_world.rs`
//! gives: `init_php_threads` can only succeed once per process.
mod support;

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use frankenrust_server::ServerConfig;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_completes_while_a_client_holds_an_idle_keep_alive_connection_open() {
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

    let stream = TcpStream::connect(addr)
        .await
        .expect("test client must be able to connect to the server under test");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("HTTP/1.1 client handshake must succeed against our own server");
    // Resolves only once the *server* closes the connection, which is the
    // second thing this test asserts.
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });

    // Two requests on the one connection: the second proves keep-alive reuse
    // is genuinely in play, so the connection this test then leaves open is an
    // idle-but-reusable one rather than an artefact of a half-finished
    // exchange.
    for attempt in 0..2 {
        sender
            .ready()
            .await
            .expect("the connection must be ready to send another request");
        let request = Request::builder()
            .method("GET")
            .uri("/index.php")
            .header("Host", "localhost")
            .body(Empty::<Bytes>::new())
            .expect("a bodiless GET request is always constructible");
        let response = tokio::time::timeout(TIMEOUT, sender.send_request(request))
            .await
            .expect("the request must complete well within the timeout")
            .expect("send_request must succeed against a live server");
        assert_eq!(response.status(), 200, "request {attempt}");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body must be fully readable")
            .to_bytes();
        assert_eq!(body.as_ref(), b"Hello World", "request {attempt}");
    }

    // `sender` is deliberately still alive here: dropping it would close the
    // connection and turn this back into the case the other two tests already
    // cover.
    let _ = shutdown_tx.send(());

    tokio::time::timeout(TIMEOUT, server)
        .await
        .expect(
            "run() must return while a client still holds an idle keep-alive connection open -- \
             a timeout here means no Connection::graceful_shutdown() was issued, the join loop \
             is waiting on a peer that will never hang up, and drain_php_threads() is unreachable",
        )
        .expect("the server task must not panic")
        .expect("run() must return Ok on a clean shutdown");

    // Shutdown must actually close the connection, not merely stop accepting
    // new ones: a server that returned from `run` while leaving established
    // sockets open would pass the assertion above for the wrong reason.
    tokio::time::timeout(TIMEOUT, driver)
        .await
        .expect("the server must have closed the idle connection during graceful shutdown")
        .expect("the client connection task must not panic");

    drop(sender);
}
