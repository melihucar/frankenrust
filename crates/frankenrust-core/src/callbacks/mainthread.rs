//! `go_frankenphp_main_thread_is_ready`, `go_frankenphp_shutdown_main_thread`,
//! `go_get_custom_php_ini`, `go_init_os_env` -- the main thread's
//! boot/shutdown hooks (`vendor/frankenphp/frankenphp.c:1621-1730`).
//! Abort-stubs for issue #7; #10 gives them real bodies.

use std::os::raw::c_char;

use frankenrust_sys::zend_array;

use super::abort_stub;

/// `frankenphp.c:1710`, inside `php_main()`. Must block for the
/// interpreter's whole life (see `docs/ARCHITECTURE.md`'s threading-model
/// section for why this is a scheduler, not a notification) -- an
/// abort-stub is a faithful placeholder precisely because it does the
/// opposite (returns immediately).
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_main_thread_is_ready() {
    abort_stub("go_frankenphp_main_thread_is_ready")
}

/// `frankenphp.c:1727`, at the very end of `php_main()`, after SAPI/TSRM
/// teardown has already run.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_shutdown_main_thread() {
    abort_stub("go_frankenphp_shutdown_main_thread")
}

/// `frankenphp.c:1681` (`ZEND_MAX_EXECUTION_TIMERS` defined) and `:1685`
/// (fallback, called with `disableTimeouts = true`).
#[unsafe(no_mangle)]
pub extern "C" fn go_get_custom_php_ini(_disable_timeouts: bool) -> *mut c_char {
    abort_stub("go_get_custom_php_ini")
}

/// `frankenphp.c:1698`, inside `php_main()`, once per process.
#[unsafe(no_mangle)]
pub extern "C" fn go_init_os_env(_main_thread_env: *mut zend_array) {
    abort_stub("go_init_os_env")
}
