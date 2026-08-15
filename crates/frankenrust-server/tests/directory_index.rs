//! Issue #187: `GET /` never entered PHP because `cgi::split_cgi_path`
//! (`cgi.rs:102`'s `split_pos`) cannot match `.php` against a one-byte path,
//! so `script_filename` came out as the document root itself -- a directory,
//! not a script. `server.rs`'s `resolve_directory_index` fixes this by
//! rewriting a request path ending in `/` to that path plus `index.php`
//! before the CGI split ever runs, mirroring upstream's Caddy `try_files`
//! rewrite (`vendor/frankenphp/caddy/php-server.go:133`).
//!
//! This is the same "one `#[tokio::test]` per test binary" constraint
//! `hello_world.rs` documents (`init_php_threads` succeeds once per
//! process): the CGI-path-variable assertions live in their own binary,
//! `tests/directory_index_cgi_vars.rs`, because they need a different
//! document root than `support::hello_document_root()`.
mod support;

use std::collections::HashMap;

use frankenrust_server::ServerConfig;

#[tokio::test]
async fn get_root_serves_index_php_instead_of_the_document_root_directory() {
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

    let (status, _headers, body) =
        tokio::time::timeout(std::time::Duration::from_secs(30), support::get(addr, "/"))
            .await
            .expect("the request must complete well within 30s");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), server)
        .await
        .expect("server must shut down within 30s -- a hang here is a lost wakeup in the dispatch/drain path")
        .expect("the server task must not panic")
        .expect("run() must return Ok on a clean shutdown");

    assert_eq!(status, 200);
    assert_eq!(
        body.as_ref(),
        b"Hello World",
        "GET / must resolve to index.php and actually execute it, the same \
         as GET /index.php in hello_world.rs -- a body-less 200 here means \
         the request never entered PHP"
    );
}
