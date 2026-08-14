//! Proves the `abort_stub` helper (crate-internal to `callbacks::mod`,
//! reached here through `callbacks::misc::abort_stub_for_test`, a test-only
//! seam) does what an unimplemented callback is supposed to do: abort the
//! process, naming the symbol, rather than let a stub return a fabricated
//! value into PHP. Without this, "all 26 symbols are defined" (the `nm`
//! check in `frankenrust-sys/tests/version.rs`) is satisfied just as well by
//! 26 bodies that silently return zero -- which is the one failure mode
//! these stubs exist to prevent, since C reads a zero from most of them as a
//! real answer.
//!
//! # Why this no longer calls `go_mercure_publish`
//!
//! This test used to call `go_mercure_publish` for its SIGABRT half, on the
//! premise that Mercure being out of scope (`docs/PORTING-NOTES.md:112`)
//! meant that one callback, unlike its 25 siblings, would stay an
//! abort-stub forever. Issue #106 found that premise wrong: upstream's own
//! no-Mercure build (`vendor/frankenphp/mercure-skip.go:12-15`) does not
//! abort anything -- it returns `(nil, 3)`, and PHP-land raises a catchable
//! `RuntimeException` from that. Aborting the whole server because a script
//! called `mercure_publish()` would be a denial-of-service upstream itself
//! does not have. #106 replaced the abort with that same `(NULL, 3)`
//! sentinel (see `callbacks::misc::go_mercure_publish`), so this test now
//! exercises the `abort_stub` helper directly through
//! [`abort_stub_for_test`] instead, and separately asserts the new Mercure
//! return value below. Testing the helper itself, rather than whichever
//! callback happens to still be stubbed, is also what keeps this file valid
//! regardless of which one that is.
//!
//! # What is still stubbed after this issue
//!
//! Every remaining abort-stub belongs to an issue that is open right now, and
//! none of them is this test's subject on purpose: each has a life
//! expectancy of a cycle or two before it is replaced in *someone else's*
//! gate run, on a file this issue does not own, and calling a
//! now-implemented callback with placeholder arguments can segfault instead
//! of cleanly demonstrating "returns instead of aborting".
//!
//! - `callbacks/thread.rs`, `callbacks/mainthread.rs`: issue #10
//! - `callbacks/servervars.rs`: issue #11
//! - `callbacks/output.rs`, `callbacks/input.rs`: issue #12
//! - `callbacks/worker.rs`: issue #14
//! - `go_log_attrs` (`callbacks/log.rs`): issue #109

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::ptr;

use frankenrust_core::callbacks::misc::{abort_stub_for_test, go_mercure_publish};

/// Set on the re-executed child. The stub aborts the whole process, so it
/// cannot be called in-process without taking the test runner down with it.
const CHILD_MARKER: &str = "FRANKENRUST_ABORT_STUB_CHILD";

const TEST_NAME: &str = "abort_stub_aborts_the_process_naming_the_symbol";

/// SIGABRT. `std::process::abort()` raises it on every platform this crate
/// builds for (`frankenrust-sys/build.rs` rejects everything but linux/macos).
const SIGABRT: i32 = 6;

/// Not a real `go_*` symbol -- picked so the assertion below is provably
/// reading the helper's own output, not coincidentally matching some real
/// callback's name.
const DUMMY_SYMBOL: &str = "not_a_real_go_callback_abort_stub_test_seam";

#[test]
fn abort_stub_aborts_the_process_naming_the_symbol() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        // `abort_stub_for_test` is a safe fn that never returns (`-> !`), so
        // no `unsafe` (and so no SAFETY comment) is owed here, and no
        // trailing `unreachable!()` either -- the compiler already knows.
        abort_stub_for_test(DUMMY_SYMBOL);
    }

    let exe = std::env::current_exe().expect("current_exe() should resolve for a test binary");
    let output = Command::new(&exe)
        // --nocapture: libtest's own capture buffer is never flushed on the
        // abort path, so without this the message we are asserting on would be
        // written into a buffer that dies with the process.
        .args(["--exact", "--nocapture", TEST_NAME])
        .env(CHILD_MARKER, "1")
        .output()
        .unwrap_or_else(|e| panic!("failed to re-run {} as a child: {e}", exe.display()));

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.signal(),
        Some(SIGABRT),
        "calling abort_stub_for_test should abort the process (SIGABRT); \
         got status {:?}. stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains(DUMMY_SYMBOL),
        "the abort message must name the symbol that was called, so a crash in a \
         PHP thread is diagnosable; stderr was:\n{stderr}"
    );
}

/// `go_mercure_publish` no longer aborts -- see this file's module doc. Its
/// zeroed return, `r0 = NULL, r1 = 3`, is the wire-format success shape C
/// switches on (`frankenphp.c:966-983`): `r1 == 3` is "not built with
/// Mercure support", the one branch that never touches `r0`. `r1 == 0` would
/// instead hit `RETURN_STR(result.r0)` and dereference a NULL `zend_string`,
/// so `r1`'s value is not incidental -- it is the one thing standing between
/// this stub and crashing PHP outright.
#[test]
fn go_mercure_publish_returns_the_not_built_sentinel_not_a_fabricated_success() {
    let result = go_mercure_publish(
        0,
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
