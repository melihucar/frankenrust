//! `go_is_context_done`, `go_putenv`, `go_schedule_opcache_reset` and
//! `go_mercure_publish` -- four callbacks that share a module because none
//! is large enough to earn its own file (`docs/ARCHITECTURE.md`'s
//! frankenrust-core section): request cancellation
//! (`vendor/frankenphp/frankenphp.c:627`), env sandboxing (`:682`, `:693`),
//! opcache reset (`:1008`) and Mercure (`:965`, explicitly out of scope --
//! see `docs/PORTING-NOTES.md:112`).
//!
//! All four are real implementations as of issue #106, not abort-stubs.
//! `go_putenv`, `go_schedule_opcache_reset` and `go_mercure_publish` are each
//! a deliberate "not implemented" answer -- env sandboxing,
//! reboot-all-threads and Mercure are all out of scope for this port -- so
//! each logs once, through `log.rs`'s facade, and returns the value C reads
//! as "feature unavailable" rather than a fabricated success. See each
//! function's doc comment for why its specific return value is correct, not
//! merely harmless.
//!
//! [`abort_stub_for_test`] is a test-only seam onto [`super::abort_stub`],
//! not a fifth callback -- see its own doc comment for why it lives here.

use std::os::raw::{c_char, c_int, c_uchar, c_ulonglong};
use std::sync::Once;

use frankenrust_sys::{go_mercure_publish_return, zend_string, zval};

use crate::context::CONTEXT_SLOTS;

use super::log::{self, Level};

/// `frankenphp.c:627`, inside `PHP_FUNCTION(frankenphp_finish_request)`.
/// Ported from `frankenphp.go:804-807`.
///
/// Deliberately stricter than upstream:
/// `phpThreads[threadIndex].frankenPHPContext()` is dereferenced unchecked
/// there (`frankenphp.go:806`), which nil-panics outside a request.
/// `thread_index` can reach here with no context installed at all --
/// extension `MINIT` output, `opcache.preload`, a module printing at
/// startup, `php_module_shutdown()` on the main thread (see issue #97) -- and
/// `true` is the *correct* answer for that case, not merely a safe one: the
/// caller reads `false` as "run `php_output_end_all()`, `php_header()` and
/// `go_frankenphp_finish_php_request()`", each of which needs a live context
/// behind it (`frankenphp.c:627-636`), while `true` makes
/// `frankenphp_finish_request()` `RETURN_FALSE` -- "there is nothing to
/// finish", which is exactly the right no-op with no context installed.
#[unsafe(no_mangle)]
pub extern "C" fn go_is_context_done(thread_index: usize) -> bool {
    CONTEXT_SLOTS.with_context(thread_index, |ctx| match ctx {
        Some(ctx) => ctx.is_done,
        None => true,
    })
}

/// Gates [`go_putenv`]'s one-time notice. One `std::sync::Once` per call
/// site, declared as its own `static` alongside the function it guards (see
/// `log.rs`'s [`log::log_once`] doc comment).
static PUTENV_LOGGED: Once = Once::new();

/// `frankenphp.c:682` (deleting a variable) and `:693` (setting one), inside
/// `PHP_FUNCTION(frankenphp_putenv)`. Ported from `env.go:26-37`.
///
/// Env sandboxing -- making a script's `putenv()` visible only inside this
/// process, never to children we spawn -- is out of scope for this port.
/// Returning `true` unconditionally rather than implementing that sandbox is
/// not a lie to PHP: C, not this function, owns `sandboxed_env`, and it is C
/// that runs `zend_hash_str_del`/`zend_hash_str_update` on it to make
/// `getenv()`/`$_ENV` inside the interpreter observe the change
/// (`frankenphp.c:683-699`) -- gated on *our* return value. Returning
/// `false` would break that `putenv()`/`getenv()` round-trip inside the
/// interpreter for every PHP script that calls `putenv()`, for no gain: the
/// only thing actually missing is propagating the change to processes this
/// server spawns after the call.
#[unsafe(no_mangle)]
pub extern "C" fn go_putenv(
    _name: *mut c_char,
    _name_len: c_int,
    _val: *mut c_char,
    _val_len: c_int,
) -> bool {
    log::log_once(
        &PUTENV_LOGGED,
        Level::WARN,
        || {
            b"putenv() was called, but frankenrust does not sandbox the process \
              environment: the change is visible to this PHP interpreter's own \
              getenv()/$_ENV, but not to any process this server spawns afterwards"
                .to_vec()
        },
        Vec::new,
    );
    true
}

/// Gates [`go_schedule_opcache_reset`]'s one-time notice.
static OPCACHE_RESET_LOGGED: Once = Once::new();

/// `frankenphp.c:1008`, inside `PHP_FUNCTION(frankenphp_opcache_reset)`.
/// Ported from `frankenphp.go:809-813`.
///
/// Upstream's own implementation is a bare `go mainThread.rebootAllThreads()`
/// -- fire-and-forget, not awaited. Rebooting every PHP thread to pick up a
/// fresh opcache is out of scope for this port, so this logs once and
/// returns; it must not block, since the caller is a live PHP thread
/// mid-request, and it does not -- there is nothing here but a `Once` check
/// and a log call, neither of which can stall.
#[unsafe(no_mangle)]
pub extern "C" fn go_schedule_opcache_reset(_thread_index: usize) {
    log::log_once(
        &OPCACHE_RESET_LOGGED,
        Level::WARN,
        || {
            b"opcache_reset() was called, but frankenrust does not implement \
              rebooting PHP threads to pick up a fresh opcache; the request \
              continues without one"
                .to_vec()
        },
        Vec::new,
    );
}

/// Gates [`go_mercure_publish`]'s one-time notice.
static MERCURE_PUBLISH_LOGGED: Once = Once::new();

/// `frankenphp.c:965`, inside `PHP_FUNCTION(mercure_publish)`. Mercure is
/// explicitly out of scope for this port (`docs/PORTING-NOTES.md:112`,
/// `docs/ARCHITECTURE.md`'s out-of-scope list).
///
/// `result.r1` is a status discriminant C switches on
/// (`frankenphp.c:966-983`): `0` is success and `RETURN_STR(result.r0)`
/// dereferences `r0` as a `zend_string *` -- a zeroed `r1` here would be read
/// as success over a NULL string and crash PHP -- `1`/`2` raise catchable
/// `RuntimeException`s, and anything else, including this function's `3`,
/// throws "FrankenPHP not built with Mercure support". Upstream's own
/// no-Mercure build returns exactly this pair
/// (`vendor/frankenphp/mercure-skip.go:12-15`: `return nil, 3`), so
/// `(NULL, 3)` here is not a fabricated stub value -- it is the oracle's own
/// answer for "Mercure is unavailable".
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
    log::log_once(
        &MERCURE_PUBLISH_LOGGED,
        Level::WARN,
        || {
            b"mercure_publish() was called, but frankenrust is not built with Mercure support"
                .to_vec()
        },
        Vec::new,
    );

    go_mercure_publish_return {
        r0: std::ptr::null_mut(),
        r1: 3,
    }
}

/// Test-only seam onto [`super::abort_stub`]. `tests/abort_stub.rs` is a
/// separate crate -- integration tests always are -- so it cannot see
/// `abort_stub`, which is `pub(crate)` in `callbacks::mod`; widening that
/// item's own visibility belongs to `mod.rs`, which this issue does not own
/// (issue #78 rewrites every callback signature there, and a one-line
/// visibility change here would be a merge conflict against that for no
/// reason). Living in this file instead costs nothing: the property under
/// test -- "an unimplemented callback aborts loudly, naming the symbol,
/// instead of returning a plausible zero" -- is a property of `abort_stub`
/// itself, so asserting it through this seam stays true even after the last
/// real abort-stub in this crate is replaced.
#[doc(hidden)]
pub fn abort_stub_for_test(symbol: &str) -> ! {
    super::abort_stub(symbol)
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::context::{CompletionSignal, RequestContext};

    fn fresh_context() -> RequestContext {
        RequestContext::new(String::new(), None, None, CompletionSignal::none())
    }

    #[test]
    fn go_is_context_done_is_true_with_no_context_installed() {
        // An index nothing else in this file (or, so far, any other test)
        // ever calls `CONTEXT_SLOTS.set` for.
        assert!(go_is_context_done(800_001));
    }

    #[test]
    fn go_is_context_done_is_false_for_a_live_context() {
        let idx = 800_002;
        CONTEXT_SLOTS.set(idx, fresh_context());
        assert!(!go_is_context_done(idx));
    }

    #[test]
    fn go_is_context_done_is_true_once_the_context_is_marked_done() {
        let idx = 800_003;
        CONTEXT_SLOTS.set(idx, fresh_context());
        CONTEXT_SLOTS.with_context_mut(idx, |ctx| {
            ctx.expect("just set").close_context();
        });
        assert!(go_is_context_done(idx));
    }

    #[test]
    fn go_putenv_returns_true_and_logs_at_most_once() {
        let (results, records) = log::capture::capture(|| {
            [
                go_putenv(ptr::null_mut(), 0, ptr::null_mut(), 0),
                go_putenv(ptr::null_mut(), 0, ptr::null_mut(), 0),
                go_putenv(ptr::null_mut(), 0, ptr::null_mut(), 0),
            ]
        });

        assert_eq!(results, [true, true, true]);
        assert!(
            records.len() <= 1,
            "go_putenv must log at most once regardless of call count, got {records:?}"
        );
    }

    #[test]
    fn go_schedule_opcache_reset_returns_promptly_and_logs_at_most_once() {
        let start = Instant::now();
        let (_, records) = log::capture::capture(|| {
            for _ in 0..3 {
                go_schedule_opcache_reset(0);
            }
        });

        assert!(
            start.elapsed() < Duration::from_secs(1),
            "go_schedule_opcache_reset must not block the calling PHP thread"
        );
        assert!(
            records.len() <= 1,
            "go_schedule_opcache_reset must log at most once regardless of call count, got {records:?}"
        );
    }

    #[test]
    fn go_mercure_publish_returns_the_out_of_scope_sentinel() {
        let result = go_mercure_publish(
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        );

        assert!(
            result.r0.is_null(),
            "r0 must stay NULL: see go_mercure_publish's doc comment for why \
             a non-NULL r1 == 3 with a non-zero r0 would still be wrong"
        );
        assert_eq!(
            result.r1, 3,
            "r1 == 3 is \"FrankenPHP not built with Mercure support\""
        );
    }

    #[test]
    fn go_mercure_publish_logs_at_most_once() {
        let (_, records) = log::capture::capture(|| {
            for _ in 0..3 {
                go_mercure_publish(
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                );
            }
        });

        assert!(
            records.len() <= 1,
            "go_mercure_publish must log at most once regardless of call count, got {records:?}"
        );
    }

    #[test]
    fn every_callback_in_this_module_tolerates_no_current_request() {
        let idx = 800_004;

        assert!(go_is_context_done(idx));
        assert!(go_putenv(ptr::null_mut(), 0, ptr::null_mut(), 0));
        go_schedule_opcache_reset(idx);

        let result = go_mercure_publish(
            idx,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        );
        assert!(result.r0.is_null());
        assert_eq!(result.r1, 3);
    }
}
