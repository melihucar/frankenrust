//! `go_ub_write`, `go_write_headers`, `go_sapi_flush` -- the SAPI module's
//! unbuffered-write / send-headers / flush hooks
//! (`vendor/frankenphp/frankenphp.c:1409-1410`). Abort-stubs for issue #7;
//! #11 gives them real bodies.

use std::os::raw::{c_char, c_int, c_uchar};

use frankenrust_sys::{go_ub_write_return, zend_llist};

use super::abort_stub;

/// `frankenphp.c:1141`, inside `frankenphp_ub_write()`
/// (`sapi_module_struct.ub_write`).
#[unsafe(no_mangle)]
pub extern "C" fn go_ub_write(
    _thread_index: usize,
    _c_buf: *mut c_char,
    _length: usize,
) -> go_ub_write_return {
    abort_stub("go_ub_write")
}

/// `frankenphp.c:1169`, inside `frankenphp_send_headers()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_write_headers(
    _thread_index: usize,
    _status: c_int,
    _headers: *mut zend_llist,
) -> bool {
    abort_stub("go_write_headers")
}

/// `frankenphp.c:1186`, inside `frankenphp_sapi_flush()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_sapi_flush(_thread_index: usize) -> c_uchar {
    abort_stub("go_sapi_flush")
}
