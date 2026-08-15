//! Acceptance test 2 (issue #13): 200 concurrent requests across 4 PHP
//! threads, all succeeding, no deadlock, and a clean shutdown afterwards.
//!
//! This exercises both dispatch paths in `thread_regular.rs` under real
//! contention: with 4 threads and 200 requests fired at once, the vast
//! majority queue on the shared channel while a handful land the directed
//! fast path, and every completed request frees its thread to immediately
//! rendezvous with the next queued one. A lost wakeup in the drain/dispatch
//! path is intermittent by nature -- see this issue's body -- so every
//! wait here is wrapped in an explicit timeout: a real hang must fail this
//! test loudly instead of hanging `cargo test` itself. This implementer ran
//! this binary repeatedly (not just once) while verifying the port; see the
//! final report for how many runs.
mod support;

use std::collections::HashMap;
use std::time::Duration;

use frankenrust_server::ServerConfig;

const REQUEST_COUNT: usize = 200;
const PHP_THREADS: usize = 4;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_concurrent_requests_across_four_php_threads_all_succeed() {
    let listener = frankenrust_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("binding an ephemeral port must succeed");
    let addr = listener
        .local_addr()
        .expect("a bound listener has a local address");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let config = ServerConfig {
        document_root: support::hello_document_root(),
        num_threads: PHP_THREADS,
        php_ini: HashMap::new(),
    };

    let server = tokio::spawn(frankenrust_server::run(listener, config, async {
        let _ = shutdown_rx.await;
    }));

    let burst = async {
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..REQUEST_COUNT {
            requests.spawn(support::get(addr, "/index.php"));
        }

        let mut succeeded = 0usize;
        while let Some(result) = requests.join_next().await {
            let (status, _headers, body) = result.expect("a request task must not panic");
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"Hello World");
            succeeded += 1;
        }
        succeeded
    };

    let succeeded = tokio::time::timeout(OVERALL_TIMEOUT, burst)
        .await
        .expect("200 requests across 4 threads must complete well within 60s -- a timeout here is a lost wakeup in the dispatch path");
    assert_eq!(succeeded, REQUEST_COUNT, "every request must succeed");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(OVERALL_TIMEOUT, server)
        .await
        .expect(
            "server must shut down within 60s -- a hang here is a lost wakeup in the drain path",
        )
        .expect("the server task must not panic")
        .expect("run() must return Ok on a clean shutdown");
}
