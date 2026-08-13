//! `go_register_server_variables`, `go_update_request_info` -- bulk
//! `$_SERVER` import and `sapi_request_info` population
//! (`vendor/frankenphp/frankenphp.c:1379`, `:355`). Real bodies for issue
//! #11: thin `//export` wrappers only, over the pure CGI/`$_SERVER` logic in
//! `crate::cgi` (the port of `cgi.go`) and the per-thread context table in
//! `crate::context` (this issue's module-layout split, since `cgi.go` itself
//! mixes cgo-calling functions with the pure helpers they use).

use std::os::raw::c_char;

use frankenrust_sys::{sapi_request_info, zval};

use crate::cgi;
use crate::context::CONTEXT_SLOTS;

/// `frankenphp.c:1379`, inside `frankenphp_register_variables()`
/// (`sapi_module_struct.register_server_variables`).
///
/// # Safety
/// Must be called only from `php_thread()` (or its worker-mode / main-thread
/// equivalents) on the OS thread that owns `thread_index`, with
/// `track_vars_array` a live, writable `$_SERVER` zval for the duration of
/// this call -- exactly the contract C already provides by construction: it
/// is the only caller, from `frankenphp_register_variables()`
/// (`frankenphp.c:1371-1383`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_register_server_variables(
    thread_index: usize,
    track_vars_array: *mut zval,
) {
    let outcome = register_server_variables_with(thread_index, |payload| {
        // Prepared-env merge (cgi.go:184-187, frankenphp_merge_with_prepared_env)
        // is out of scope: `fc.env` (PreparedEnv) is not part of this
        // issue's RequestContext -- see issue #11's "out of scope" section.
        //
        // SAFETY: called on the PHP thread that owns `thread_index`, from
        // inside `frankenphp_register_variables()` (frankenphp.c:1371-1383)
        // while a Zend request is active, so `track_vars_array` is the live
        // $_SERVER zval PHP just passed us and `frankenphp_strings`
        // (populated at main-thread boot) is initialised. No slot guard is
        // alive here -- see `register_server_variables_with`.
        unsafe { cgi::register_server_vars(payload, track_vars_array) }
    });

    if outcome.is_err() {
        // One of `shim.c`'s trampolines caught a `zend_bailout()` and turned
        // it into an ordinary return so that every Rust frame between here
        // and the C call could unwind normally. They all have: the statement
        // above has returned, so the payload, the closure, and every guard
        // and buffer either of them owned are dropped. What is left on the
        // stack is this frame, holding two `Copy` arguments and one `Result`
        // of two fieldless types -- zero drop glue, nothing to leak, no
        // borrow to invalidate. Only now is it safe to resume the unwind PHP
        // was in the middle of.
        //
        // SAFETY: `frankenrust_bailout` is `zend_bailout()`. Two
        // preconditions, both established above: (1) we are on the PHP thread
        // that owns `thread_index` with its TSRM cache updated (this
        // function's own contract), so `EG(bailout)` resolves; (2) it is
        // non-NULL, because a trampoline only reports a bailout when it
        // intercepted one that was already heading for an enclosing
        // `zend_try` -- `php_request_startup`'s, which returns FAILURE and
        // sends `frankenphp.c:1512-1515` back to `frankenphp_php_thread`'s
        // `zend_first_try` at `:1504`, exactly where upstream's uncaught
        // bailout would have landed. It does not return.
        unsafe { frankenrust_sys::frankenrust_bailout() };
    }
}

/// Collects the `$_SERVER` payload under the context slot's lock, releases
/// the lock, and only then runs `register` -- which is where every call into
/// PHP happens.
///
/// That ordering is the whole point of this function existing separately from
/// the `extern "C"` wrapper (and is what its test exercises): any of the
/// register calls can `zend_error_noreturn(E_ERROR, "Allowed memory size ...
/// exhausted")`, which ends in `zend_bailout()` -- a `longjmp` to a
/// `zend_catch` above our frames that runs no Rust destructors. A slot guard
/// held across it would be leaked and never released, and the first thing C
/// does after catching (`frankenphp.c:1592` ->
/// `go_frankenphp_after_script_execution`) is clear that very slot. See
/// [`crate::context::ContextSlots`] for the full argument.
///
/// Releasing the lock closes *that* hazard; the register calls themselves are
/// kept from `longjmp`ing over any Rust frame by `shim.c`'s
/// `zend_try`/`zend_catch` trampolines, which turn a bailout into the
/// `Err(cgi::Bailout)` this function propagates. See
/// [`crate::cgi::ServerVarsPayload`]'s doc comment.
///
/// Returning that verdict rather than acting on it is the point: the caller
/// re-raises only after this function has returned, i.e. after `payload` and
/// `register` are dropped. Nothing here may swallow it -- a bailout means the
/// engine has gone fatal and PHP is waiting to finish an unwind we
/// intercepted. (`Result` is already `#[must_use]`, which is what makes
/// "swallow it" a compile error rather than a convention.)
fn register_server_variables_with(
    thread_index: usize,
    register: impl FnOnce(&cgi::ServerVarsPayload) -> Result<(), cgi::Bailout>,
) -> Result<(), cgi::Bailout> {
    let payload = CONTEXT_SLOTS.with_context(thread_index, |slot| {
        // Upstream dereferences `thread.frankenPHPContext()` unconditionally
        // here (cgi.go:176-177) and would itself panic on a nil context --
        // this call site is only ever reached from inside
        // php_request_startup's $_SERVER population, by which point a
        // context always exists. A panic here would be UB across the
        // `extern "C"` boundary (docs/PORTING-NOTES.md), so on the
        // should-be-unreachable case we log and no-op rather than abort the
        // whole process for it.
        let Some(ctx) = slot else {
            eprintln!(
                "frankenrust: go_register_server_variables: no RequestContext for thread {thread_index}"
            );
            return None;
        };

        cgi::collect_server_vars(ctx)
    });

    // Both slot guards are released at the end of the statement above.
    let Some(payload) = payload else {
        return Ok(());
    };

    register(&payload)
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

    // Unlike `go_register_server_variables` above, this one *does* run its
    // work under the slot lock, and must: it appends to the context's arena,
    // which the context owns. That is sound because `cgi::update_request_info`
    // makes no call into PHP at all -- it only writes plain fields of
    // `SG(request_info)` and allocates through Rust -- so there is no
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

    use crate::context::{Request, RequestContext};

    /// The reviewed defect this pins down: `go_register_server_variables`
    /// used to hold the context slot's `Mutex` (and the slot table's
    /// `RwLock` read guard) across `frankenphp_register_server_vars` and one
    /// `frankenphp_register_known_variable`/`_variable_safe` per header. Any
    /// of those can exhaust `memory_limit` -- ordinary in worker mode, where
    /// the resident worker script counts against the same budget -- and
    /// `zend_error_noreturn(E_ERROR, ...)` ends in `zend_bailout()`, a
    /// `longjmp` that runs no Rust destructor. The guards would be leaked and
    /// the slot locked forever, right before C's crash-recovery path
    /// (`frankenphp.c:1592` -> `go_frankenphp_after_script_execution`) tries
    /// to clear that very slot.
    ///
    /// So: the register step must run with the slot free. The probe thread
    /// stands in for the post-bailout cleanup; on the buggy shape it blocks
    /// forever and this test fails on the timeout.
    #[test]
    fn register_step_runs_with_the_context_slot_unlocked() {
        // CONTEXT_SLOTS is a process-global and the test harness runs tests
        // in parallel, so this index is reserved to this test alone.
        const THREAD_INDEX: usize = 40;

        let (tx, _rx) = mpsc::channel();
        let mut request = Request::new("GET", "/index.php", "");
        request.host = "example.com".to_string();
        CONTEXT_SLOTS.set(
            THREAD_INDEX,
            RequestContext::new("/var/www".to_string(), None, Some(request), tx)
                .expect("the default split path is valid"),
        );

        let mut register_ran = false;
        let outcome = register_server_variables_with(THREAD_INDEX, |payload| {
            register_ran = true;
            assert_eq!(payload.known.script_name, b"/index.php");

            let (done_tx, done_rx) = mpsc::channel();
            let probe = std::thread::spawn(move || {
                CONTEXT_SLOTS.clear(THREAD_INDEX);
                let _ = done_tx.send(());
            });
            assert!(
                done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
                "the context slot must be free while the register step calls into PHP: \
                 a zend_bailout() out of those calls never runs Rust destructors, so a \
                 guard held here would wedge the slot permanently"
            );
            probe.join().expect("probe thread panicked");
            Ok(())
        });

        assert_eq!(outcome, Ok(()));
        assert!(
            register_ran,
            "the register step must run when a context is installed"
        );
        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn register_step_is_skipped_without_a_context() {
        // Reserved to this test alone -- see the test above.
        const THREAD_INDEX: usize = 41;

        let mut register_ran = false;
        let outcome = register_server_variables_with(THREAD_INDEX, |_| {
            register_ran = true;
            Ok(())
        });
        assert_eq!(outcome, Ok(()));
        assert!(!register_ran, "no context installed: nothing to register");
    }

    /// The second reviewed defect this pins down: the register calls used to
    /// go straight into `frankenphp_register_server_vars` /
    /// `_known_variable` / `_variable_safe`, any of which can exhaust
    /// `memory_limit` and end in `zend_bailout()` -- a `longjmp` over every
    /// Rust frame between the C call and `php_request_startup`'s `zend_catch`.
    /// Rust calls that undefined behaviour whatever those frames own, and
    /// they owned plenty: the payload, the per-header `Vec<u8>` key, the
    /// closure.
    ///
    /// The fix moves the `setjmp` below every Rust frame (`shim.c`) and makes
    /// the bailout a *value*, so the re-raise can be deferred until the stack
    /// holds nothing that needs dropping. This test is the Rust half of that
    /// contract: a bailout reported from inside the register step must come
    /// back out through an ordinary `return`, with everything the step owned
    /// already destroyed by the time the caller sees it.
    ///
    /// The C half -- that `zend_try`/`zend_catch` really does intercept the
    /// `longjmp` -- is not unit-testable here: `zend_try` writes
    /// `EG(bailout)` through the TSRM cache, so any test touching it without
    /// a live PHP thread segfaults rather than fails (the same reason issue
    /// #11 forbids calling `frankenphp_register_server_vars` from a test).
    #[test]
    fn a_caught_bailout_returns_with_the_register_step_fully_dropped() {
        // Reserved to this test alone -- see the tests above.
        const THREAD_INDEX: usize = 42;

        let (tx, _rx) = mpsc::channel();
        let mut request = Request::new("GET", "/index.php", "");
        request.host = "example.com".to_string();
        CONTEXT_SLOTS.set(
            THREAD_INDEX,
            RequestContext::new("/var/www".to_string(), None, Some(request), tx)
                .expect("the default split path is valid"),
        );

        // Stands in for every droppable value that is live in the register
        // step's frames at the moment C bails out -- the ones the old code
        // would have let the `longjmp` skip.
        let canary = std::sync::Arc::new(());
        let observer = std::sync::Arc::downgrade(&canary);

        let outcome = register_server_variables_with(THREAD_INDEX, move |_payload| {
            let _live_across_the_bailout = canary;
            Err(cgi::Bailout)
        });

        assert_eq!(
            outcome,
            Err(cgi::Bailout),
            "a caught bailout must propagate to the caller, never be swallowed: \
             PHP is mid-unwind and someone has to re-raise it"
        );
        assert!(
            observer.upgrade().is_none(),
            "everything the register step owned must be dropped before the caller \
             re-raises with frankenrust_bailout(); anything still alive here would \
             be leaked by the longjmp that follows"
        );

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }
}
