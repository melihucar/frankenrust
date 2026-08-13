//! `go_frankenphp_store_force_kill_slot`, `go_frankenphp_before_script_execution`,
//! `go_frankenphp_after_script_execution`, `go_frankenphp_clear_force_kill_slot`
//! and `go_frankenphp_on_thread_shutdown` -- the per-thread lifecycle
//! callbacks `php_thread()` calls
//! (`vendor/frankenphp/frankenphp.c:1471-1619`). Abort-stubs for issue #7;
//! #10 gives them real bodies.

use std::os::raw::{c_char, c_int};

use frankenrust_sys::force_kill_slot;

use super::abort_stub;

/// `frankenphp.c:299`, inside `frankenphp_register_thread_for_kill()`,
/// itself called once at the top of `php_thread()` (`frankenphp.c:1497`) on
/// the thread the slot belongs to.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_store_force_kill_slot(
    _thread_index: usize,
    _slot: force_kill_slot,
) {
    abort_stub("go_frankenphp_store_force_kill_slot")
}

/// `frankenphp.c:1506`, the condition of `php_thread()`'s main loop: blocks
/// until a script is ready, returns its name, or `NULL` to end the thread.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_before_script_execution(_thread_index: usize) -> *mut c_char {
    abort_stub("go_frankenphp_before_script_execution")
}

/// `frankenphp.c:1562` (normal path) and `:1591` (`zend_catch` path).
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_after_script_execution(_thread_index: usize, _exit_status: c_int) {
    abort_stub("go_frankenphp_after_script_execution")
}

/// `frankenphp.c:1598`, immediately before `ts_free_thread()` -- must run
/// first, since that call frees the TSRM storage the slot's `EG()`
/// pointers point into.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_clear_force_kill_slot(_thread_index: usize) {
    abort_stub("go_frankenphp_clear_force_kill_slot")
}

/// `frankenphp.c:1607`, only on the healthy-shutdown path of `php_thread()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_on_thread_shutdown(_thread_index: usize) {
    abort_stub("go_frankenphp_on_thread_shutdown")
}
