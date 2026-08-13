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
    CONTEXT_SLOTS.with_context(thread_index, |slot| {
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
            return;
        };

        if let Some(request) = ctx.request.as_ref() {
            if let Some(vars) = cgi::compute_server_vars(ctx) {
                // SAFETY: called on the PHP thread that owns `thread_index`,
                // from inside `frankenphp_register_variables()`
                // (frankenphp.c:1371-1383) while a Zend request is active,
                // so `track_vars_array` is the live $_SERVER zval PHP just
                // passed us and `frankenphp_strings` (populated at
                // main-thread boot) is initialised.
                unsafe {
                    cgi::register_known_server_vars(&vars, track_vars_array);
                }
            }
            // SAFETY: same call-site guarantee as above.
            unsafe {
                cgi::add_headers_to_server(&request.headers, track_vars_array);
            }
        }

        // Prepared-env merge (cgi.go:184-187, frankenphp_merge_with_prepared_env)
        // is out of scope: `fc.env` (PreparedEnv) is not part of this
        // issue's RequestContext -- see issue #11's "out of scope" section.
    });
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
