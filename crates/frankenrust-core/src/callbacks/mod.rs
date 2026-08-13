//! The 26 `go_*` symbols `vendor/frankenphp/frankenphp.c` calls back into,
//! declared for C in `crates/frankenrust-sys/include/_cgo_export.h` and
//! defined here as `#[unsafe(no_mangle)] pub extern "C" fn`s. Grouped by
//! functional area (`docs/ARCHITECTURE.md`'s frankenrust-core section)
//! rather than 1:1 with an upstream Go file.
//!
//! This module only declares the submodules below -- the split across them
//! is issue #7's pre-declared module layout, frozen so #10, #11, #12 and
//! #14 can each fill in bodies without touching files another agent is
//! editing at the same time.
//!
//! Every function in every submodule is an abort-stub for this issue: the
//! object code for `frankenphp.c` references all 26 symbols unconditionally,
//! so the link fails unless something defines them, but no real behaviour
//! exists yet. Enumerated in issue #7's final report so nobody mistakes them
//! for finished work; #10 (thread.rs), #11 (worker.rs/servervars.rs/
//! input.rs/output.rs -- the request path), #12 and #14 replace them.

pub mod input;
pub mod log;
pub mod mainthread;
pub mod misc;
pub mod output;
pub mod servervars;
pub mod thread;
pub mod worker;

/// Writes `symbol` to stderr and aborts the process. This is the body every
/// callback in this module has for issue #7: fail loudly, by name, the first
/// time C actually calls one, rather than pretend to succeed.
///
/// `abort()` rather than `panic!`/`unimplemented!`: the callers are all plain
/// `extern "C"` fns, across which unwinding is undefined behaviour, so a panic
/// would only reach the compiler's abort-on-unwind shim -- and it would run the
/// panic hook first, on a stack PHP owns and whose signal mask
/// (`frankenphp.c:225-231` blocks SIGUSR1/SIGUSR2/SIGALRM process-wide) is not
/// ours. Aborting directly is the same outcome with none of that machinery, and
/// it is what issue #7 asks these stubs to do.
///
/// Returning a fabricated value instead is not an option worth having: every
/// one of these is a SAPI hook or a userland function's implementation, so a
/// plausible-looking zero would be read by PHP as a real answer (an empty
/// request body, a successful write, a thread that may proceed).
pub(crate) fn abort_stub(symbol: &str) -> ! {
    eprintln!(
        "frankenrust: FATAL: {symbol} was called, but it is still issue #7's \
         abort-stub -- no implementation exists yet. See docs/PORTING-NOTES.md \
         and issue #7's pre-declared module layout for which later issue \
         (#10/#11/#12/#14) replaces it."
    );
    std::process::abort();
}
