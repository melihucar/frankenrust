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

/// Boots `config.num_threads` PHP threads as regular threads (upstream's
/// `frankenphp.go:316-329`, with `workerThreadCount` fixed at zero -- worker
/// mode is #14's), then serves `listener` until `shutdown` resolves, then
/// drains every PHP thread before returning.
///
/// `shutdown` is a caller-supplied future rather than a hardcoded signal
/// source so tests can trigger it explicitly (`tokio::signal::ctrl_c()` for
/// the real binary, a oneshot for a test harness that wants a clean,
/// deterministic stop after its own requests complete).
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

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote_addr) = accepted?;
                let document_root = Arc::clone(&document_root);
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        handle(req, Arc::clone(&document_root), remote_addr)
                    });
                    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                        eprintln!("frankenrust: connection error: {err}");
                    }
                });
            }
            () = &mut shutdown => break,
        }
    }

    // Stop accepting; let every in-flight connection finish on its own.
    drop(listener);
    while connections.join_next().await.is_some() {}

    // `drain_php_threads` (thread.rs, #10) is idempotent-safe to call once
    // every connection above has returned its response -- no request can
    // still be in flight past this point.
    frankenrust_core::thread::drain_php_threads();

    Ok(())
}

/// Binds `addr` and hands the listener to [`run`]. Split out of `run` so a
/// caller (tests, `main.rs`) can pick an ephemeral port (`:0`) and read back
/// the bound address before the accept loop starts.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
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

    let core_request = build_request(&parts, body_bytes, remote_addr);

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
