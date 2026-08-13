//! `frankenrust_collect_server_vars`, `go_update_request_info` -- bulk
//! `$_SERVER` import and `sapi_request_info` population
//! (`vendor/frankenphp/frankenphp.c:1379`, `:355`). Real bodies for issue
//! #11: thin `//export` wrappers only, over the pure CGI/`$_SERVER` logic in
//! `crate::cgi` (the port of `cgi.go`) and the per-thread context table in
//! `crate::context` (this issue's module-layout split, since `cgi.go` itself
//! mixes cgo-calling functions with the pure helpers they use).
//!
//! # Why `go_register_server_variables` is not here
//!
//! It is the one `go_*` callback whose C-ABI entry point is written in C, in
//! `crates/frankenrust-sys/shim.c`, and this module supplies only the half of
//! it that touches no PHP API ([`frankenrust_collect_server_vars`]).
//!
//! Every function that callback calls -- `frankenphp_register_server_vars`,
//! `frankenphp_register_known_variable`, `frankenphp_register_variable_safe`
//! -- allocates through the Zend *request* allocator, which on `memory_limit`
//! exhaustion ends in `zend_bailout()`: a `longjmp` to a `zend_catch` above
//! the callback. Go tolerates that jump crossing a live cgo frame; Rust has no
//! defined behaviour for it crossing a Rust frame, and -- the trap the first
//! two revisions of this file fell into -- catching the bailout in C and
//! re-raising it from Rust does not help, because the re-raise is itself a
//! `longjmp` out of the Rust callback frame. Dropping every payload first
//! removes the leak, not the undefined behaviour.
//!
//! So the split is structural rather than defensive: Rust computes and
//! *returns*, then C registers with no Rust frame anywhere between
//! `zend_bailout()` and `php_request_startup`'s `zend_catch`. No `zend_try` of
//! our own is involved and PHP's control flow is bit-for-bit upstream's. See
//! `shim.c`'s header comment and issue #75.

use std::os::raw::c_char;

use frankenrust_sys::{frankenrust_server_vars_batch, sapi_request_info};

use crate::cgi;
use crate::context::CONTEXT_SLOTS;

/// The Rust half of `go_register_server_variables` (`cgi.go:174-188`), called
/// from `shim.c` before it makes its first Zend call. Declared in
/// `crates/frankenrust-sys/include/frankenrust_shim.h`.
///
/// Returns `false` -- leaving `*out` untouched, so C registers nothing -- when
/// there is no context for `thread_index`, or the context has no request. The
/// latter is upstream's `if fc.request != nil` guard (`cgi.go:179`), which
/// gates both the known variables and the headers.
///
/// Two properties this function must keep, both of which are why it exists
/// separately from the C entry point rather than being inlined into it:
///
/// 1. **It makes no call into PHP.** It therefore cannot bail out, so no
///    `longjmp` can cross this frame. Holding the slot lock for the whole body
///    is safe *because* of that, and would not be otherwise -- see
///    [`crate::context::ContextSlots`]'s "one rule for callers": a leaked slot
///    guard wedges the thread's own crash-recovery path
///    (`frankenphp.c:1591` -> `go_frankenphp_after_script_execution` clears
///    this very slot).
/// 2. **It returns before the registration starts,** and hands out only
///    pointers into memory the [`crate::context::RequestContext`] owns, so a
///    bailout during the registration leaks nothing and dangles nothing.
///
/// # Safety
/// Must be called only from `shim.c`'s `go_register_server_variables`, which C
/// calls from `frankenphp_register_variables()` (`frankenphp.c:1371-1383`) on
/// the PHP thread that owns `thread_index`. `out` must be a writable,
/// suitably aligned `frankenrust_server_vars_batch` (`shim.c` passes the
/// address of one of its own locals) that stays alive for this call.
///
/// The pointers written into `*out` are valid until `thread_index`'s context
/// slot is cleared or replaced -- which, on both of upstream's reclaim paths,
/// happens strictly after C is finished with them: regular mode clears at
/// `go_frankenphp_after_script_execution` (`threadregular.go:129-133`), worker
/// mode at `go_frankenphp_finish_worker_request` (`threadworker.go:314-318`),
/// and both run long after `frankenphp_register_variables` has returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frankenrust_collect_server_vars(
    thread_index: usize,
    out: *mut frankenrust_server_vars_batch,
) -> bool {
    if out.is_null() {
        return false;
    }

    CONTEXT_SLOTS.with_context_mut(thread_index, |slot| {
        // Upstream dereferences `thread.frankenPHPContext()` unconditionally
        // here (cgi.go:176-177) and would itself panic on a nil context --
        // this call site is only ever reached from inside
        // php_request_startup's $_SERVER population, by which point a
        // context always exists. A panic here would be UB across the
        // `extern "C"` boundary (docs/PORTING-NOTES.md), so on the
        // should-be-unreachable case we log and register nothing rather than
        // abort the whole process for it.
        let Some(ctx) = slot else {
            eprintln!(
                "frankenrust: go_register_server_variables: no RequestContext for thread {thread_index}"
            );
            return false;
        };

        let Some(batch) = cgi::build_server_vars_batch(ctx) else {
            return false;
        };

        // Installing before taking the C view is what makes the pointers
        // outlive this frame: they target the context's copy, not the local.
        //
        // Prepared-env merge (cgi.go:185-187,
        // frankenphp_merge_with_prepared_env) is out of scope: `fc.env`
        // (PreparedEnv) is not part of this issue's RequestContext -- see
        // issue #11's "out of scope" section.
        let c_batch = ctx.install_server_vars(batch);

        // SAFETY: `out` is non-null (checked above) and, per this function's
        // contract, points at a writable, aligned, live
        // `frankenrust_server_vars_batch` owned by the C caller's frame.
        // `write` (not `*out = `) because the destination is C's
        // uninitialised local: an assignment would first *drop* whatever is
        // nominally there. `frankenrust_server_vars_batch` is plain old data
        // with no drop glue, so the two are equivalent today -- `write` is
        // what states the intent and stays correct if that ever changes.
        unsafe { out.write(c_batch) };
        true
    })
}

/// `frankenphp.c:355`, inside `frankenphp_update_request_context()`, called
/// at the top of every request (`frankenphp.c:1509`) and every worker
/// request (`frankenphp.c:563`).
///
/// # Safety
/// Must be called only from `php_thread()` on the OS thread that owns
/// `thread_index`, with `info` either NULL or pointing at that thread's own
/// `SG(request_info)` (TSRM-resident, so exclusively this thread's) for the
/// duration of this call -- again the contract C already provides: the only
/// caller is `frankenphp_update_request_context()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_update_request_info(
    thread_index: usize,
    info: *mut sapi_request_info,
) -> *mut c_char {
    if info.is_null() {
        return std::ptr::null_mut();
    }

    // Runs under the slot lock, and must: it appends to the context's arena,
    // which the context owns. That is sound for the same reason
    // `frankenrust_collect_server_vars` above is -- `cgi::update_request_info`
    // makes no call into PHP at all, only writes plain fields of
    // `SG(request_info)` and allocates through Rust, so there is no
    // `zend_bailout()` that could `longjmp` past the guard's destructor.
    CONTEXT_SLOTS.with_context_mut(thread_index, |slot| {
        let Some(ctx) = slot else {
            eprintln!(
                "frankenrust: go_update_request_info: no RequestContext for thread {thread_index}"
            );
            return std::ptr::null_mut();
        };
        // SAFETY: `info` is `&SG(request_info)` (frankenphp.c:355), passed
        // by the PHP thread that owns `thread_index` and exclusively ours
        // for the duration of this call -- it is TSRM-resident state on the
        // calling thread, per docs/PORTING-NOTES.md's "called only from
        // php_thread() on the thread that owns thread_index". `info` was
        // just checked non-null above.
        let info = unsafe { &mut *info };
        cgi::update_request_info(ctx, info)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::time::Duration;

    use crate::context::{CompletionSignal, Request, RequestContext};

    /// Installs a context on `thread_index` and returns the batch
    /// `shim.c` would have received.
    fn collect(thread_index: usize, request: Request) -> Option<frankenrust_server_vars_batch> {
        CONTEXT_SLOTS.set(
            thread_index,
            RequestContext::new(
                "/var/www".to_string(),
                None,
                Some(request),
                CompletionSignal::none(),
            )
            .expect("the default split path is valid"),
        );

        let mut batch = frankenrust_server_vars_batch::default();
        // SAFETY: stands in for `shim.c`'s call. `&mut batch` is a writable,
        // aligned, live `frankenrust_server_vars_batch` on this frame, and the
        // context installed above lives in `thread_index`'s slot for the rest
        // of the test, so the pointers written into it stay valid.
        let filled = unsafe { frankenrust_collect_server_vars(thread_index, &mut batch) };
        filled.then_some(batch)
    }

    /// The reviewed defect this pins down: the callback used to hold the
    /// context slot's `Mutex` (and the slot table's `RwLock` read guard)
    /// across `frankenphp_register_server_vars` and one
    /// `frankenphp_register_known_variable`/`_variable_safe` per header. Any
    /// of those can exhaust `memory_limit` -- ordinary in worker mode, where
    /// the resident worker script counts against the same budget -- and
    /// `zend_error_noreturn(E_ERROR, ...)` ends in `zend_bailout()`, a
    /// `longjmp` that runs no Rust destructor. The guards would be leaked and
    /// the slot locked forever, right before C's crash-recovery path
    /// (`frankenphp.c:1591` -> `go_frankenphp_after_script_execution`) tries
    /// to clear that very slot.
    ///
    /// The registration now happens in `shim.c` *after* this function has
    /// returned, so the guards cannot still be held -- but "cannot" is a claim
    /// about this function's return, which is exactly what a test can check.
    /// The probe thread stands in for the post-bailout cleanup; if any guard
    /// outlived the call it would block forever and this test would fail on
    /// the timeout.
    #[test]
    fn the_slot_is_free_once_collection_returns() {
        // CONTEXT_SLOTS is a process-global and the test harness runs tests
        // in parallel, so this index is reserved to this test alone.
        const THREAD_INDEX: usize = 40;

        let mut request = Request::new("GET", "/index.php", "");
        request.host = "example.com".to_string();
        let batch = collect(THREAD_INDEX, request).expect("a context is installed");
        assert_eq!(batch.vars.script_name_len, "/index.php".len());

        let (done_tx, done_rx) = mpsc::channel();
        let probe = std::thread::spawn(move || {
            CONTEXT_SLOTS.clear(THREAD_INDEX);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the context slot must be free once collection returns: the C caller \
             goes straight on to call into PHP, and a zend_bailout() out of those \
             calls never runs Rust destructors, so a guard still held here would \
             wedge the slot permanently"
        );
        probe.join().expect("probe thread panicked");
    }

    #[test]
    fn collection_reports_nothing_without_a_context() {
        // Reserved to this test alone -- see the test above.
        const THREAD_INDEX: usize = 41;

        let mut batch = frankenrust_server_vars_batch::default();
        // SAFETY: `&mut batch` is writable, aligned and live; no context is
        // installed on this index, which is the case under test.
        let filled = unsafe { frankenrust_collect_server_vars(THREAD_INDEX, &mut batch) };

        assert!(
            !filled,
            "no context installed: C must register nothing rather than read an \
             untouched batch"
        );
        assert_eq!(batch.num_headers, 0, "the batch must be left untouched");
    }

    /// A NULL `out` must be reported as "nothing to register" rather than
    /// written through. `shim.c` never passes one, but this callback is
    /// `extern "C"` and a null-deref here is a segfault inside PHP's request
    /// startup, which is the least diagnosable place in the port.
    #[test]
    fn collection_rejects_a_null_out_pointer() {
        // Reserved to this test alone -- see the tests above.
        const THREAD_INDEX: usize = 42;

        let mut request = Request::new("GET", "/index.php", "");
        request.host = "example.com".to_string();
        CONTEXT_SLOTS.set(
            THREAD_INDEX,
            RequestContext::new(
                "/var/www".to_string(),
                None,
                Some(request),
                CompletionSignal::none(),
            )
            .expect("the default split path is valid"),
        );

        // SAFETY: the null case is precisely what is under test, and the
        // function documents that it checks before writing.
        let filled = unsafe { frankenrust_collect_server_vars(THREAD_INDEX, std::ptr::null_mut()) };
        assert!(!filled);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    /// The property the `shim.c` split rests on: by the time C dereferences
    /// them, the Rust frame that produced these pointers is gone, so they must
    /// target memory the `RequestContext` owns -- not a local, and not
    /// something a `zend_bailout()` would leak.
    #[test]
    fn the_batch_stays_readable_after_collection_returns() {
        // Reserved to this test alone -- see the tests above.
        const THREAD_INDEX: usize = 43;

        let mut request =
            Request::new("GET", "/index.php", "").with_header("X-Foo", b"bar".to_vec());
        request.host = "example.com".to_string();
        let batch = collect(THREAD_INDEX, request).expect("a context is installed");

        assert_eq!(batch.num_headers, 1);
        // SAFETY: `frankenrust_collect_server_vars` has returned, and the
        // context it wrote these pointers out of is still installed in
        // THREAD_INDEX's slot (cleared at the end of this test), so every
        // pointer below is live. That is the invariant under test.
        unsafe {
            let host = std::slice::from_raw_parts(
                batch.vars.http_host as *const u8,
                batch.vars.http_host_len,
            );
            assert_eq!(host, b"example.com");

            let header = &*batch.headers;
            assert!(header.known_key.is_null(), "X-Foo is not pre-interned");
            assert_eq!(
                std::ffi::CStr::from_ptr(header.key).to_bytes(),
                b"HTTP_X_FOO"
            );
            let value = std::slice::from_raw_parts(header.value as *const u8, header.value_len);
            assert_eq!(value, b"bar");
        }

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }
}
