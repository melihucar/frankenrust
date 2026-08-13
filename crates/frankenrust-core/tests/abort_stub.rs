//! Proves the issue #7 abort-stubs do what the issue asks them to do: abort
//! the process, naming the symbol, rather than returning a fabricated value
//! into PHP. Without this, "all 26 symbols are defined" (the `nm` check in
//! `frankenrust-sys/tests/version.rs`) is satisfied just as well by 26 bodies
//! that silently return zero -- which is the one failure mode these stubs
//! exist to prevent, since C reads a zero from most of them as a real answer.
//!
//! `go_mercure_publish` is the subject on purpose. `docs/PORTING-NOTES.md:112`
//! puts Mercure out of scope for this port ("out of scope: return a stub"), so
//! unlike its 25 siblings it is *not* replaced by #10/#11/#12/#14 and this test
//! stays valid -- and stays honest -- after they land. Asserting on a stub that
//! is scheduled to be implemented would make this a test whose only future is
//! to be deleted.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::ptr;

/// Set on the re-executed child. The stub aborts the whole process, so it
/// cannot be called in-process without taking the test runner down with it.
const CHILD_MARKER: &str = "FRANKENRUST_ABORT_STUB_CHILD";

const TEST_NAME: &str = "abort_stub_aborts_the_process_naming_the_symbol";

/// SIGABRT. `std::process::abort()` raises it on every platform this crate
/// builds for (`frankenrust-sys/build.rs` rejects everything but linux/macos).
const SIGABRT: i32 = 6;

#[test]
fn abort_stub_aborts_the_process_naming_the_symbol() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        // `go_mercure_publish` is a safe `extern "C" fn`, so no `unsafe` (and
        // so no SAFETY comment) is owed here: `abort_stub` aborts before any
        // argument is read, which is exactly what the null pointers below
        // assert. This call does not return.
        go_mercure_publish_with_nulls();
        unreachable!("go_mercure_publish returned instead of aborting");
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
        "calling the go_mercure_publish stub should abort the process (SIGABRT); \
         got status {:?}. stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("go_mercure_publish"),
        "the abort message must name the symbol that was called, so a crash in a \
         PHP thread is diagnosable; stderr was:\n{stderr}"
    );
}

/// Split out so the child path stays a single expression and the null
/// arguments are documented once. Never returns.
fn go_mercure_publish_with_nulls() {
    frankenrust_core::callbacks::misc::go_mercure_publish(
        0,               // threadIndex
        ptr::null_mut(), // topics: *mut zval
        ptr::null_mut(), // data: *mut zend_string
        0,               // private
        ptr::null_mut(), // id: *mut zend_string
        ptr::null_mut(), // typ: *mut zend_string
        0,               // retry
    );
}
