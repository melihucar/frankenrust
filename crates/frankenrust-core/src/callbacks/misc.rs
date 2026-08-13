//! `go_is_context_done`, `go_putenv`, `go_schedule_opcache_reset`,
//! `go_mercure_publish` -- four callbacks that share a module because none
//! is large enough to earn its own file (`docs/ARCHITECTURE.md`'s
//! frankenrust-core section): request cancellation
//! (`vendor/frankenphp/frankenphp.c:627`), env sandboxing (`:682`, `:693`),
//! opcache reset (`:1008`) and Mercure (`:965`, explicitly out of scope --
//! see `docs/PORTING-NOTES.md`). Abort-stubs for issue #7; #10/#12 give the
//! first three real bodies. `go_mercure_publish` stays a stub permanently.

use std::os::raw::{c_char, c_int, c_uchar, c_ulonglong};

use frankenrust_sys::{go_mercure_publish_return, zend_string, zval};

use super::abort_stub;

/// `frankenphp.c:627`, inside `PHP_FUNCTION(frankenphp_finish_request)`.
#[unsafe(no_mangle)]
pub extern "C" fn go_is_context_done(_thread_index: usize) -> bool {
    abort_stub("go_is_context_done")
}

/// `frankenphp.c:682` (deleting a variable) and `:693` (setting one),
/// inside `PHP_FUNCTION(frankenphp_putenv)`.
#[unsafe(no_mangle)]
pub extern "C" fn go_putenv(
    _name: *mut c_char,
    _name_len: c_int,
    _val: *mut c_char,
    _val_len: c_int,
) -> bool {
    abort_stub("go_putenv")
}

/// `frankenphp.c:1008`, inside `PHP_FUNCTION(frankenphp_opcache_reset)`.
#[unsafe(no_mangle)]
pub extern "C" fn go_schedule_opcache_reset(_thread_index: usize) {
    abort_stub("go_schedule_opcache_reset")
}

/// `frankenphp.c:965`, inside `PHP_FUNCTION(mercure_publish)`. Mercure is
/// explicitly out of scope for this port (`docs/PORTING-NOTES.md`'s "Out of
/// scope" list); this stays an abort-stub even after issue #7 replaces its
/// 25 siblings.
#[unsafe(no_mangle)]
pub extern "C" fn go_mercure_publish(
    _thread_index: usize,
    _topics: *mut zval,
    _data: *mut zend_string,
    _private: c_uchar,
    _id: *mut zend_string,
    _typ: *mut zend_string,
    _retry: c_ulonglong,
) -> go_mercure_publish_return {
    abort_stub("go_mercure_publish")
}
