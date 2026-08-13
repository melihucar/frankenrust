//! `go_frankenphp_worker_handle_request_start`,
//! `go_frankenphp_finish_worker_request`, `go_frankenphp_finish_php_request`
//! -- worker-mode request handoff (`vendor/frankenphp/frankenphp.c:830-920`,
//! `:623-637`). Abort-stubs for issue #7; #12 gives them real bodies.

use frankenrust_sys::{go_frankenphp_worker_handle_request_start_return, zval};

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
/// backs userland `fastcgi_finish_request()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_finish_php_request(_thread_index: usize) {
    abort_stub("go_frankenphp_finish_php_request")
}
