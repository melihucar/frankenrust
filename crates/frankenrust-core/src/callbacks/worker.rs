//! `go_frankenphp_worker_handle_request_start`,
//! `go_frankenphp_finish_worker_request`, `go_frankenphp_finish_php_request`
//! -- worker-mode request handoff (`vendor/frankenphp/frankenphp.c:830-920`,
//! `:623-637`). The first two are still issue #7's abort-stubs, left for #14.
//! `go_frankenphp_finish_php_request` has a real body (#169), ported from
//! `vendor/frankenphp/threadworker.go:328-341` -- it backs userland
//! `fastcgi_finish_request()` / `frankenphp_finish_request()`, which is
//! ordinary userland PHP reachable in **regular** mode, not just from a
//! worker script, so it could not wait for the rest of #14.

use frankenrust_sys::{go_frankenphp_worker_handle_request_start_return, zval};

use crate::context::CONTEXT_SLOTS;

use super::abort_stub;

/// `frankenphp.c:852`, inside `PHP_FUNCTION(frankenphp_handle_request)` --
/// parks until the next request is ready to hand to the worker script's
/// callback, or shutdown.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_worker_handle_request_start(
    _thread_index: usize,
) -> go_frankenphp_worker_handle_request_start_return {
    abort_stub("go_frankenphp_worker_handle_request_start")
}

/// `frankenphp.c:911`, after the worker's PHP callback returns.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_finish_worker_request(_thread_index: usize, _retval: *mut zval) {
    abort_stub("go_frankenphp_finish_worker_request")
}

/// `frankenphp.c:634`, inside `PHP_FUNCTION(frankenphp_finish_request)` --
/// backs userland `fastcgi_finish_request()`. Ported from
/// `threadworker.go:328-341`, minus the debug-level log line (`log.rs`'s
/// facade is #106's, not this callback's to invent a new call site for) and
/// upstream's own log-message construction on top of `fc.request.RequestURI`
/// -- neither is observable behaviour a caller can depend on.
///
/// Deliberately does **not** clear `thread_index`'s context slot: see
/// [`crate::context::RequestContext::close_context`]'s doc comment for why
/// upstream leaves the context installed here (a script that calls
/// `fastcgi_finish_request()` keeps running, and keeps writing through
/// `SG(request_info)`, afterwards) and why clearing it early would free an
/// arena the interpreter still reads. `RegularThread::after_script_execution`
/// (`thread_regular.rs`) stays the one place that clears this slot.
///
/// `close_context` only flips a bool and fires the completion signal --
/// see [`crate::context::ContextSlots`]'s two rules for callers, which that
/// method satisfies (it does not call into PHP, block, or re-enter
/// `CONTEXT_SLOTS`) and which this callback must not violate either.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_finish_php_request(thread_index: usize) {
    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        if let Some(ctx) = ctx {
            ctx.close_context();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::callbacks::output::go_ub_write;
    use crate::context::{CompletionSignal, FlushError, Request, RequestContext, ResponseSink};

    // See output.rs's `tests` module doc comment for why these indices must
    // be small and distinct from every other file's slice of the
    // process-global `CONTEXT_SLOTS`: misc.rs uses 1-4, output.rs uses
    // 100-116, input.rs uses 120-127, servervars.rs uses 60-76. This file's
    // own indices live in a disjoint range.
    const IDX_FINISH_CLOSES_AND_IS_IDEMPOTENT: usize = 130;
    const IDX_FINISH_THEN_UB_WRITE_REPORTS_CACHED_SNAPSHOT: usize = 131;

    /// A [`ResponseSink`] that only records what was written to it -- enough
    /// to prove a post-finish `go_ub_write` never reaches the sink at all,
    /// which is the whole point of `IDX_FINISH_THEN_UB_WRITE_REPORTS_CACHED_SNAPSHOT`.
    struct FakeSink {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl ResponseSink for FakeSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }

        fn add_header(&mut self, _name: &str, _value: &[u8]) {}

        fn clear_headers(&mut self) {}

        fn write_status(&mut self, _status: u16) {}

        fn flush(&mut self) -> Result<(), FlushError> {
            Ok(())
        }
    }

    #[test]
    fn finish_php_request_closes_the_context_fires_the_signal_once_and_leaves_the_slot_installed() {
        let idx = IDX_FINISH_CLOSES_AND_IS_IDEMPOTENT;
        let fire_count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&fire_count);
        let ctx = RequestContext::new(
            String::new(),
            None,
            None,
            CompletionSignal::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        );
        CONTEXT_SLOTS.set(idx, ctx);

        go_frankenphp_finish_php_request(idx);

        assert_eq!(
            fire_count.load(Ordering::SeqCst),
            1,
            "the completion signal must fire exactly once"
        );
        let is_done = CONTEXT_SLOTS.with_context(idx, |ctx| ctx.expect("still installed").is_done);
        assert!(is_done, "close_context() must have marked the context done");

        // A second call must be a no-op: close_context() is idempotent, so
        // the (already-consumed) signal must not fire again and is_done must
        // simply stay true.
        go_frankenphp_finish_php_request(idx);
        assert_eq!(
            fire_count.load(Ordering::SeqCst),
            1,
            "a second finish call must not fire the signal again"
        );

        let still_installed = CONTEXT_SLOTS.with_context(idx, |ctx| ctx.is_some());
        CONTEXT_SLOTS.clear(idx);
        assert!(
            still_installed,
            "go_frankenphp_finish_php_request must not clear the slot -- the \
             script keeps running and keeps writing through SG(request_info) \
             after fastcgi_finish_request(); the regular-thread reclaim point \
             (RegularThread::after_script_execution) stays the one place that \
             clears it"
        );
    }

    #[test]
    fn finish_php_request_with_no_context_does_not_abort() {
        // No slot installed at all -- must be a no-op, not the abort-stub
        // behaviour this callback used to have.
        go_frankenphp_finish_php_request(IDX_FINISH_CLOSES_AND_IS_IDEMPOTENT + 1000);
    }

    #[test]
    fn go_ub_write_after_finish_php_request_discards_the_write_and_reports_the_cached_snapshot() {
        // The finish-request.php regression (vendor/frankenphp/testdata/
        // finish-request.php): once fastcgi_finish_request() has run, a
        // later write must be discarded and must report the *cached*
        // client_had_closed snapshot taken at close-time, not a fresh
        // client_has_closed() check -- which would read "aborted" for
        // virtually every post-finish write, since firing the completion
        // signal is what lets the awaiting HTTP handler return and cancel
        // the request on the transport side. This is the same distinction
        // output.rs's `go_ub_write_after_close_context_discards_the_write_and_reports_the_cached_snapshot`
        // pins directly against `close_context()`; this test pins it
        // through the real entry point PHP userland actually calls.
        let idx = IDX_FINISH_THEN_UB_WRITE_REPORTS_CACHED_SNAPSHOT;
        let request = Request::new("GET", b"/".to_vec());
        let cancelled = request.cancelled.clone();
        let mut ctx =
            RequestContext::new(String::new(), None, Some(request), CompletionSignal::none());
        let writes = Arc::new(Mutex::new(Vec::new()));
        ctx.response_sink = Some(Box::new(FakeSink {
            writes: Arc::clone(&writes),
        }));
        CONTEXT_SLOTS.set(idx, ctx);

        go_frankenphp_finish_php_request(idx);
        let client_had_closed =
            CONTEXT_SLOTS.with_context(idx, |ctx| ctx.expect("still installed").client_had_closed);
        assert!(
            !client_had_closed,
            "close_context() must have snapshotted 'not closed' before this \
             test flips the live flag"
        );

        // The client disconnects *after* the request finished -- exactly
        // what a normal fastcgi_finish_request() followed by continued
        // script execution looks like.
        cancelled.store(true, Ordering::SeqCst);

        let mut payload = b"still writing after finish".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());
        CONTEXT_SLOTS.clear(idx);

        assert!(
            writes.lock().unwrap().is_empty(),
            "a post-finish write must be discarded, never reach the sink"
        );
        assert_eq!(
            result.r0,
            payload.len(),
            "a post-finish write must report the full length, not the \
             (discarded) sink result"
        );
        assert!(
            !result.r1,
            "must report the CACHED client_had_closed (false), not the \
             live, now-true flag a fresh client_has_closed() would report"
        );
    }
}
