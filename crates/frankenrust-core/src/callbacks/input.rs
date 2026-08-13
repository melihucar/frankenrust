//! `go_read_post`, `go_read_cookies`, `go_apache_request_headers` -- request
//! body/cookie/header hooks (`vendor/frankenphp/frankenphp.c:1191-1196`,
//! `:762-776`). Abort-stubs for issue #7; #11 gives them real bodies.

use std::os::raw::c_char;

use frankenrust_sys::go_apache_request_headers_return;

use super::abort_stub;

/// `frankenphp.c:1192`, inside `frankenphp_read_post()`
/// (`sapi_module_struct.read_post`).
#[unsafe(no_mangle)]
pub extern "C" fn go_read_post(
    _thread_index: usize,
    _c_buf: *mut c_char,
    _count_bytes: usize,
) -> usize {
    abort_stub("go_read_post")
}

/// `frankenphp.c:1196`, inside `frankenphp_read_cookies()`
/// (`sapi_module_struct.read_cookies`).
#[unsafe(no_mangle)]
pub extern "C" fn go_read_cookies(_thread_index: usize) -> *mut c_char {
    abort_stub("go_read_cookies")
}

/// `frankenphp.c:766`, inside `PHP_FUNCTION(frankenphp_request_headers)` --
/// backs userland `apache_request_headers()` / `getallheaders()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_apache_request_headers(
    _thread_index: usize,
) -> go_apache_request_headers_return {
    abort_stub("go_apache_request_headers")
}
