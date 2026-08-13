//! `go_log`, `go_log_attrs` -- the SAPI error log hook and the structured
//! `frankenphp_log()` userland function
//! (`vendor/frankenphp/frankenphp.c:1385-1387`, `:985-1004`). Abort-stubs
//! for issue #7; #10 gives them real bodies.

use std::os::raw::{c_char, c_int};

use frankenrust_sys::{zend_long, zend_string, zval};

use super::abort_stub;

/// `frankenphp.c:1386`, inside `frankenphp_log_message()`
/// (`sapi_module_struct.log_message`).
#[unsafe(no_mangle)]
pub extern "C" fn go_log(_thread_index: usize, _message: *mut c_char, _level: c_int) {
    abort_stub("go_log")
}

/// `frankenphp.c:998`, inside `PHP_FUNCTION(frankenphp_log)`, and `:1586`,
/// on `php_thread()`'s unhealthy-thread (`zend_catch`) path.
#[unsafe(no_mangle)]
pub extern "C" fn go_log_attrs(
    _thread_index: usize,
    _message: *mut zend_string,
    _c_level: zend_long,
    _c_attrs: *mut zval,
) -> *mut c_char {
    abort_stub("go_log_attrs")
}
