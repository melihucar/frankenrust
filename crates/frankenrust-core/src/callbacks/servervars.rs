//! `go_register_server_variables`, `go_update_request_info` -- bulk
//! `$_SERVER` import and `sapi_request_info` population
//! (`vendor/frankenphp/frankenphp.c:1379`, `:355`). Abort-stubs for issue
//! #7; #11 gives them real bodies.

use std::os::raw::c_char;

use frankenrust_sys::{sapi_request_info, zval};

use super::abort_stub;

/// `frankenphp.c:1379`, inside `frankenphp_register_variables()`
/// (`sapi_module_struct.register_server_variables`).
#[unsafe(no_mangle)]
pub extern "C" fn go_register_server_variables(_thread_index: usize, _track_vars_array: *mut zval) {
    abort_stub("go_register_server_variables")
}

/// `frankenphp.c:355`, inside `frankenphp_update_request_context()`, called
/// at the top of every request (`frankenphp.c:1509`) and every worker
/// request (`frankenphp.c:563`).
#[unsafe(no_mangle)]
pub extern "C" fn go_update_request_info(
    _thread_index: usize,
    _info: *mut sapi_request_info,
) -> *mut c_char {
    abort_stub("go_update_request_info")
}
