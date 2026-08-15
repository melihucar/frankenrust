//! Server bootstrap: boots the PHP thread pool as regular threads, then runs
//! the hyper HTTP/1.1 accept loop -- the Rust side of
//! `vendor/frankenphp/frankenphp.go:316-329` (`initPHPThreads` +
//! `convertToRegularThread` in a loop) and `ServeHTTP`
//! (`frankenphp.go:396-428`).
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use frankenrust_core::cgi;
use frankenrust_core::context::{CompletionSignal, RequestContext};
use frankenrust_core::thread::{get_inactive_php_thread, init_php_threads, MaxThreads};
use frankenrust_core::thread_regular::{
    convert_to_regular_thread, handle_request_with_regular_php_threads,
};

use crate::request::build_request;
use crate::response::{build_response, internal_error_response, reject_response};
use crate::sink::{BufferedResponseSink, SharedResponseBuffer};

pub struct ServerConfig {
    pub document_root: PathBuf,
    pub num_threads: usize,
    pub php_ini: HashMap<String, String>,
}

/// How long shutdown lets already-accepted connections finish after every one
/// of them has been told to stop reusing itself
/// ([`http1::Connection::graceful_shutdown`]), before abandoning the
/// stragglers and moving on to the PHP drain.
///
/// A deadline is required, not decorative: `graceful_shutdown` waits for the
/// *in-flight request* to finish, and an in-flight request here means a PHP
/// script, which can run arbitrarily long. Without a bound, one runaway script
/// wedges `run` forever and `drain_php_threads` -- which has its own 30s grace
/// period followed by a force-kill (`thread.rs`'s `drain_generation`) -- is
/// never reached to cut it short. Matched to that same 30s so the two phases
/// read as one budget rather than two arbitrary numbers.
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Backoff bounds for a *recoverable* `accept()` failure, mirroring the
/// 5ms-doubling-to-1s retry in Go's `net/http.Server.Serve` -- which is what
/// hosts upstream's `ServeHTTP`, and which likewise does not let a transient
/// accept error kill the server.
const INITIAL_ACCEPT_BACKOFF: Duration = Duration::from_millis(5);
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// Boots `config.num_threads` PHP threads as regular threads (upstream's
/// `frankenphp.go:316-329`, with `workerThreadCount` fixed at zero -- worker
/// mode is #14's), then serves `listener` until `shutdown` resolves, then
/// drains every PHP thread before returning.
///
/// `shutdown` is a caller-supplied future rather than a hardcoded signal
/// source so tests can trigger it explicitly (`tokio::signal::ctrl_c()` for
/// the real binary, a oneshot for a test harness that wants a clean,
/// deterministic stop after its own requests complete).
///
/// "Drains every PHP thread before returning" is unconditional, including on
/// the `Err` return: an accept error this loop cannot retry still unwinds
/// through the drain rather than leaving the pool live with TSRM never torn
/// down.
pub async fn run(
    listener: TcpListener,
    config: ServerConfig,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    init_php_threads(
        config.num_threads,
        MaxThreads::Fixed(config.num_threads),
        config.php_ini,
    )
    .map_err(std::io::Error::other)?;

    for _ in 0..config.num_threads {
        let claim = get_inactive_php_thread()
            .expect("exactly config.num_threads inactive slots were just booted");
        convert_to_regular_thread(claim)
            .expect("a freshly claimed inactive slot must accept a handler");
    }

    let document_root = Arc::new(config.document_root);
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    // Flips `false` -> `true` exactly once, at shutdown. Every connection task
    // holds a receiver and reacts by calling `graceful_shutdown()` on its own
    // `Connection`. A `watch` rather than a broadcast because the payload is a
    // latch, not a stream: a connection accepted before the flip is guaranteed
    // to observe it (it subscribed first), and one accepted after cannot exist
    // -- the accept loop has already broken.
    let (graceful_tx, _) = tokio::sync::watch::channel(false);
    let mut accept_backoff = INITIAL_ACCEPT_BACKOFF;
    // An accept error we cannot retry still has to unwind through the drain
    // below rather than `?`-ing straight out of `run` and leaving the whole
    // PHP pool live with TSRM never torn down.
    let mut fatal: Option<std::io::Error> = None;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote_addr) = match accepted {
                    Ok(accepted) => {
                        accept_backoff = INITIAL_ACCEPT_BACKOFF;
                        accepted
                    }
                    Err(err) if is_transient_accept_error(&err) => {
                        // ECONNABORTED (client RST between SYN and accept),
                        // EINTR, or fd exhaustion: all recoverable, none of
                        // them a reason to kill a running server. Back off so
                        // a persistent EMFILE cannot spin the accept loop hot.
                        eprintln!(
                            "frankenrust: accept failed ({err}); retrying in {accept_backoff:?}"
                        );
                        tokio::select! {
                            () = tokio::time::sleep(accept_backoff) => {}
                            () = &mut shutdown => break,
                        }
                        accept_backoff = (accept_backoff * 2).min(MAX_ACCEPT_BACKOFF);
                        continue;
                    }
                    Err(err) => {
                        fatal = Some(err);
                        break;
                    }
                };
                let document_root = Arc::clone(&document_root);
                let mut graceful_rx = graceful_tx.subscribe();
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        handle(req, Arc::clone(&document_root), remote_addr)
                    });
                    let mut conn =
                        std::pin::pin!(http1::Builder::new().serve_connection(io, service));
                    // hyper's `Connection` future resolves only when the
                    // *peer* closes or the connection errors, and HTTP/1.1
                    // keep-alive is the default with no idle timeout -- so an
                    // idle-but-open connection (every browser, pooled client
                    // and reverse proxy between requests) would otherwise pin
                    // this task forever and make the join below unbounded.
                    // `graceful_shutdown()` is the API that fixes exactly
                    // that: finish the in-flight request, then close.
                    let mut signalled = false;
                    loop {
                        tokio::select! {
                            result = conn.as_mut() => {
                                if let Err(err) = result {
                                    eprintln!("frankenrust: connection error: {err}");
                                }
                                return;
                            }
                            // Disarmed after firing: `changed()` on an
                            // already-observed value would park forever, but
                            // if `graceful_tx` is ever dropped it returns
                            // `Err` immediately, which without the guard would
                            // spin this loop hot.
                            changed = graceful_rx.changed(), if !signalled => {
                                let _ = changed;
                                signalled = true;
                                conn.as_mut().graceful_shutdown();
                            }
                        }
                    }
                });
            }
            // `JoinSet` never removes a finished task's entry on its own --
            // only `join_next`/`try_join_next`/`abort_all`/`Drop` do -- so
            // without this arm the set grows by one task allocation per
            // connection served, for the entire lifetime of the server.
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(err) = joined {
                    eprintln!("frankenrust: connection task failed: {err}");
                }
            }
            () = &mut shutdown => break,
        }
    }

    // Stop accepting, then tell every live connection to finish its in-flight
    // request and close instead of waiting on a peer that may never hang up.
    drop(listener);
    let _ = graceful_tx.send(true);

    let drained = tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        eprintln!(
            "frankenrust: {} connection(s) still in flight after {CONNECTION_DRAIN_TIMEOUT:?}; \
             abandoning them so the PHP pool can be drained",
            connections.len()
        );
        // Cancelling a connection task drops its `handle` future; the
        // `spawn_blocking` dispatch it may have orphaned still owns the
        // `RequestContext`, and the `drain_php_threads` below force-kills the
        // PHP thread executing it. See #174 for the residual window this
        // shares with an ordinary mid-request client disconnect.
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    // `drain_php_threads` (thread.rs, #10) is reached on *every* exit from the
    // loop above -- clean shutdown, transient-accept-error-then-shutdown, and
    // fatal accept error alike -- which is what this function's doc comment
    // promises. The `fatal` detour below exists only so that promise holds.
    frankenrust_core::thread::drain_php_threads();

    match fatal {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Whether an `accept()` failure is one a running server should retry rather
/// than die on.
///
/// Go's `net/http.Server.Serve` -- the host of upstream's `ServeHTTP` --
/// retries temporary accept errors with a backoff instead of returning, and
/// this is the list that matters in practice: a peer that RSTs between SYN and
/// `accept()` (`ECONNABORTED`, and `ECONNRESET` on the platforms that report it
/// that way), a signal (`EINTR`), and process- or system-wide fd exhaustion
/// (`EMFILE`/`ENFILE`), which is transient because the fds in use are freed as
/// connections close.
///
/// `EMFILE`/`ENFILE` are matched on the raw errno because `std` maps neither to
/// a named [`std::io::ErrorKind`] on stable -- they arrive as
/// `ErrorKind::Uncategorized`, which is itself unnameable, so `raw_os_error` is
/// the only stable way to tell them from a genuine failure.
fn is_transient_accept_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    ) || matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// Binds `addr` and hands the listener to [`run`]. Split out of `run` so a
/// caller (tests, `main.rs`) can pick an ephemeral port (`:0`) and read back
/// the bound address before the accept loop starts.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Rewrites a request path ending in `/` to that same path plus
/// `index.php`, in place. No-op otherwise.
///
/// Upstream does this one layer above `frankenphp` proper, in Caddy's
/// `try_files` (`vendor/frankenphp/caddy/php-server.go:133`): by the time a
/// request reaches `splitCgiPath` (`cgi.go`), the URL has already been
/// rewritten. FrankenRust has no Caddy layer, so `handle` plays that role
/// itself, calling this immediately before `cgi::split_cgi_path` -- the same
/// position relative to the split that upstream's rewrite occupies.
///
/// This is deliberately the no-stat slice of `tryFiles`, not the full
/// three-way probe: only "path ends in `/`" is handled. No filesystem stat,
/// no static-file branch, no configurable index filename -- out of scope for
/// the thin slice (`docs/PORTING-NOTES.md:175`, `docs/ARCHITECTURE.md:401`),
/// and matching what `docker/pasir.Dockerfile:37-42` and
/// `bench/harness/config/Caddyfile.matched` both already document as the
/// comparison point.
///
/// Verified empirically, not assumed: run against the pinned upstream image
/// (`dunglas/frankenphp@sha256:4b0713dd...`, `corpus.toml`'s
/// `[targets.upstream]`) with `vendor/frankenphp/testdata` mounted at
/// `/app/public`, `GET /` returns
/// `DOCUMENT_URI=/index.php SCRIPT_NAME=/index.php
/// SCRIPT_FILENAME=/app/public/index.php PATH_INFO=(empty)` but
/// `REQUEST_URI=/` -- unrewritten (`GET /dirindex/` behaves the same way one
/// level down: `DOCUMENT_URI=/dirindex/index.php`,
/// `REQUEST_URI=/dirindex/`). So only the CGI-split input is rewritten here;
/// `raw_target` (and therefore `REQUEST_URI` / `context.rs`'s
/// `request_uri`, which copies `raw_target` verbatim) must stay untouched,
/// which is exactly what mutating `core_request.path` and not
/// `core_request.raw_target` gives us.
fn resolve_directory_index(path: &mut Vec<u8>) {
    if path.ends_with(b"/") {
        path.extend_from_slice(b"index.php");
    }
}

/// The `ServeHTTP` port (`frankenphp.go:396-428`): builds a
/// [`RequestContext`], validates it, dispatches it to a regular PHP thread,
/// awaits completion, and renders the response through this crate's single
/// [`build_response`] seam.
async fn handle(
    req: HyperRequest<Incoming>,
    document_root: Arc<PathBuf>,
    remote_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        // A body that fails to fully arrive (e.g. the client hung up
        // mid-upload) has no upstream analogue worth reproducing precisely
        // here -- treat it as an empty body rather than failing the whole
        // connection.
        Err(_) => Bytes::new(),
    };

    let mut core_request = build_request(&parts, body_bytes, remote_addr);
    resolve_directory_index(&mut core_request.path);

    // #107 (CGI path splitting inside `RequestContext::new`) is open and
    // unlanded as of this issue: `doc_uri`/`path_info`/`script_name`/
    // `script_filename` start empty (`context.rs`'s `RequestContext::new`
    // doc comment) and nothing else computes them. `cgi::split_cgi_path` is
    // called here, immediately after construction, exactly where upstream's
    // `NewRequestWithContext` calls `splitCgiPath` (`context.go:109`).
    // Remove this block (and this comment) once #107 lands and the
    // constructor fills these in itself.
    let document_root_bytes = path_bytes(&document_root);
    let paths = cgi::split_cgi_path(&document_root_bytes, None, &core_request.path);

    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    // `RequestContext::document_root` is a `String`, not a byte string, so a
    // document root whose path bytes are not valid UTF-8 (ordinary on Linux)
    // is corrupted here before `cgi::split_cgi_path` ever sees it. Filed as
    // #171; fixing it means widening `RequestContext::document_root` itself
    // (`context.rs`), out of this issue's lane.
    let mut ctx = RequestContext::new(
        String::from_utf8_lossy(&document_root_bytes).into_owned(),
        None,
        Some(core_request),
        CompletionSignal::new(move || {
            let _ = completion_tx.send(());
        }),
    );
    ctx.doc_uri = paths.doc_uri;
    ctx.path_info = paths.path_info;
    ctx.script_name = paths.script_name;
    ctx.script_filename = paths.script_filename;

    if let Err(rejected) = ctx.validate() {
        return Ok(reject_response(&rejected));
    }

    let shared = Arc::new(Mutex::new(SharedResponseBuffer::default()));
    ctx.response_sink = Some(Box::new(BufferedResponseSink::new(Arc::clone(&shared))));

    // The dispatch itself is a blocking call on its shared-channel fallback
    // path (see `thread_regular.rs`'s doc comment) -- it must never run on a
    // tokio worker thread, so it is handed to `spawn_blocking`. Completion is
    // awaited separately, over the oneshot: `docs/PORTING-NOTES.md:130-147`'s
    // diagram draws these as two distinct steps for us, where upstream's
    // single blocking channel receive did both at once.
    //
    // This costs one blocking-pool thread per *queued* request, not per
    // executing one -- tokio's default `max_blocking_threads` (512) is a
    // concurrency ceiling upstream's goroutine-per-request model does not
    // have. Filed as #173; removing it means giving dispatch an async
    // counterpart, which crosses the tokio-free boundary
    // `docs/ARCHITECTURE.md` draws around `frankenrust-core` and so is its
    // own design decision, not a call-site fix.
    let dispatched = tokio::task::spawn_blocking(move || {
        handle_request_with_regular_php_threads(ctx);
    })
    .await;

    if dispatched.is_err() {
        return Ok(internal_error_response());
    }

    // A disconnect here (no thread ever received the request) is the only
    // way this resolves to `Err`; the response is then built from whatever
    // the still-default buffer holds, which reads as an empty 200 rather
    // than hanging the connection.
    let _ = completion_rx.await;

    let buffer = shared.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(build_response(&buffer))
}

#[cfg(unix)]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_directory_index_appends_index_php_to_a_trailing_slash() {
        let mut root = b"/".to_vec();
        resolve_directory_index(&mut root);
        assert_eq!(root, b"/index.php");

        let mut sub = b"/sub/".to_vec();
        resolve_directory_index(&mut sub);
        assert_eq!(sub, b"/sub/index.php");
    }

    #[test]
    fn resolve_directory_index_leaves_a_non_directory_path_alone() {
        let mut script = b"/index.php".to_vec();
        resolve_directory_index(&mut script);
        assert_eq!(script, b"/index.php");

        let mut sub_script = b"/sub/hello.php".to_vec();
        resolve_directory_index(&mut sub_script);
        assert_eq!(sub_script, b"/sub/hello.php");
    }

    /// The accept loop's survival hinges on this classification, and the two
    /// errnos that matter most (`EMFILE`/`ENFILE`) are unreachable from an
    /// integration test without exhausting the test process's own fd table --
    /// so they are pinned here instead. A regression that dropped either back
    /// into the fatal branch would kill a live server on fd pressure.
    #[test]
    fn fd_exhaustion_and_aborted_handshakes_are_retryable_accept_errors() {
        for errno in [libc::EMFILE, libc::ENFILE] {
            assert!(
                is_transient_accept_error(&std::io::Error::from_raw_os_error(errno)),
                "errno {errno} (fd exhaustion) must not terminate the accept loop"
            );
        }
        for kind in [
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
        ] {
            assert!(
                is_transient_accept_error(&std::io::Error::from(kind)),
                "{kind:?} must not terminate the accept loop"
            );
        }
    }

    #[test]
    fn a_genuinely_broken_listener_is_not_retryable() {
        // Retrying forever on an unrecoverable listener would spin instead of
        // shutting down, so the transient set must stay a set, not a blanket.
        for kind in [
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::AddrNotAvailable,
        ] {
            assert!(
                !is_transient_accept_error(&std::io::Error::from(kind)),
                "{kind:?} must surface as a fatal accept error"
            );
        }
    }
}
