//! Issue #187: asserts the four CGI path variables `resolve_directory_index`
//! (`server.rs`) is responsible for getting right, against values observed
//! empirically from the pinned upstream image
//! (`dunglas/frankenphp@sha256:4b0713ddad6ca7eb21eb82ac6bdb7cb41de5192a930b615d89af6e15d74e82f8`,
//! `tests/conformance/corpus.toml`'s `[targets.upstream]`), run by hand with
//! `vendor/frankenphp/testdata` mounted at `/app/public` and a script that
//! dumps `$_SERVER`:
//!
//!   GET /            -> DOCUMENT_URI=/index.php       PATH_INFO=(empty)
//!                        SCRIPT_NAME=/index.php        SCRIPT_FILENAME=/app/public/index.php
//!                        (REQUEST_URI=/, unrewritten -- not asserted here,
//!                        already covered by context.rs's request_uri tests)
//!   GET /dirindex/   -> DOCUMENT_URI=/dirindex/index.php PATH_INFO=(empty)
//!                        SCRIPT_NAME=/dirindex/index.php
//!                        SCRIPT_FILENAME=/app/public/dirindex/index.php
//!   GET /vars.php/a/ -> DOCUMENT_URI=/vars.php         PATH_INFO=/a
//!                        SCRIPT_NAME=/vars.php          SCRIPT_FILENAME=/app/public/vars.php
//!                        (REQUEST_URI=/vars.php/a/; no index.php anywhere)
//!
//! i.e. for a path ending in `/` every CGI path variable is computed as
//! though the request had asked for `index.php` directly, and only
//! `REQUEST_URI` stays unrewritten -- but a path already carrying a `.php`
//! split point is claimed by `tryFiles`' *first* entry and is not touched at
//! all. This file's fixture (`tests/fixtures/directory_index/`) mirrors that
//! shape: `sub/index.php` so `GET /sub/` can be checked against `tryFiles`'
//! second entry (`{path}/index.php`), and `vars.php` so `GET /vars.php/a/`
//! can be checked against the first.
//!
//! A separate binary from `tests/directory_index.rs` -- see that file's doc
//! comment for why (`init_php_threads` succeeds once per process, and this
//! needs a different document root).
mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use frankenrust_server::ServerConfig;

fn fixture_document_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/directory_index")
}

/// Parses the fixture script's `KEY=value\n`-per-line body into a map.
fn parse_vars(body: &[u8]) -> HashMap<String, String> {
    String::from_utf8(body.to_vec())
        .expect("the fixture script only ever emits ASCII")
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[tokio::test]
async fn get_root_and_get_sub_resolve_cgi_path_vars_as_if_index_php_were_requested() {
    let listener = frankenrust_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("binding an ephemeral port must succeed");
    let addr = listener
        .local_addr()
        .expect("a bound listener has a local address");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let root = fixture_document_root();
    let config = ServerConfig {
        document_root: root.clone(),
        num_threads: 1,
        php_ini: HashMap::new(),
    };

    let server = tokio::spawn(frankenrust_server::run(listener, config, async {
        let _ = shutdown_rx.await;
    }));

    let timeout = std::time::Duration::from_secs(30);
    let (root_status, _root_headers, root_body) =
        tokio::time::timeout(timeout, support::get(addr, "/"))
            .await
            .expect("GET / must complete well within 30s");
    let (sub_status, _sub_headers, sub_body) =
        tokio::time::timeout(timeout, support::get(addr, "/sub/"))
            .await
            .expect("GET /sub/ must complete well within 30s");
    let (path_info_status, _path_info_headers, path_info_body) =
        tokio::time::timeout(timeout, support::get(addr, "/vars.php/a/"))
            .await
            .expect("GET /vars.php/a/ must complete well within 30s");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(timeout, server)
        .await
        .expect("server must shut down within 30s -- a hang here is a lost wakeup in the dispatch/drain path")
        .expect("the server task must not panic")
        .expect("run() must return Ok on a clean shutdown");

    assert_eq!(root_status, 200);
    let root_vars = parse_vars(&root_body);
    let root_str = root.to_str().expect("fixture root must be valid UTF-8");
    assert_eq!(
        root_vars.get("DOCUMENT_URI").map(String::as_str),
        Some("/index.php")
    );
    assert_eq!(root_vars.get("PATH_INFO").map(String::as_str), Some(""));
    assert_eq!(
        root_vars.get("SCRIPT_NAME").map(String::as_str),
        Some("/index.php")
    );
    assert_eq!(
        root_vars.get("SCRIPT_FILENAME").map(String::as_str),
        Some(format!("{root_str}/index.php")).as_deref()
    );

    assert_eq!(sub_status, 200);
    let sub_vars = parse_vars(&sub_body);
    assert_eq!(
        sub_vars.get("DOCUMENT_URI").map(String::as_str),
        Some("/sub/index.php")
    );
    assert_eq!(sub_vars.get("PATH_INFO").map(String::as_str), Some(""));
    assert_eq!(
        sub_vars.get("SCRIPT_NAME").map(String::as_str),
        Some("/sub/index.php")
    );
    assert_eq!(
        sub_vars.get("SCRIPT_FILENAME").map(String::as_str),
        Some(format!("{root_str}/sub/index.php")).as_deref()
    );

    // The guard, from the other side. `tryFiles` is ordered and its first
    // entry -- `{http.request.uri.path}` behind a `file` matcher with
    // `SplitPath: [".php"]` (`php-server.go:133` + `:165-171`) -- claims any
    // path carrying a `.php` split point, so upstream never reaches the
    // directory-index entry for one. Same oracle run as the header comment:
    //
    //   GET /vars.php/a/ -> DOCUMENT_URI=/vars.php  SCRIPT_NAME=/vars.php
    //                        SCRIPT_FILENAME=/app/public/vars.php
    //                        PATH_INFO=/a  REQUEST_URI=/vars.php/a/
    //
    // Without the guard `resolve_directory_index` rewrites this to
    // `/vars.php/a/index.php`, `split_pos` splits at the *first* `.php`, and
    // `PATH_INFO` becomes `/a/index.php` -- a segment the client never sent,
    // delivered to the application's router. `SCRIPT_NAME`/`SCRIPT_FILENAME`
    // stay correct either way, so PHP still runs and still answers 200:
    // nothing but this assertion catches it.
    assert_eq!(path_info_status, 200);
    let path_info_vars = parse_vars(&path_info_body);
    assert_eq!(
        path_info_vars.get("DOCUMENT_URI").map(String::as_str),
        Some("/vars.php")
    );
    assert_eq!(
        path_info_vars.get("SCRIPT_NAME").map(String::as_str),
        Some("/vars.php")
    );
    assert_eq!(
        path_info_vars.get("SCRIPT_FILENAME").map(String::as_str),
        Some(format!("{root_str}/vars.php")).as_deref()
    );
    // Upstream reports `/a`, we report `/a/`: Caddy's entry-1 rewrite
    // canonicalises the trailing slash away and we have no URI-canonicalising
    // layer. That one byte predates this rewrite -- it is what
    // `split_cgi_path` alone produced for this path before
    // `resolve_directory_index` existed -- and is filed as #196, which will
    // flip this to `/a`. What is pinned here is that nothing is *appended*.
    assert_eq!(
        path_info_vars.get("PATH_INFO").map(String::as_str),
        Some("/a/"),
        "PATH_INFO must carry only what the client sent; `/a/index.php` here \
         means the directory-index rewrite fired on a path upstream's \
         tryFiles claims at entry 1"
    );
}
