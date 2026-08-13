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

/// Panics naming `symbol`, which -- because every function in this module
/// has C linkage -- unwinds straight into Rust's FFI-unwind guard and
/// aborts the process instead of continuing across an `extern "C"` boundary
/// with a real Rust panic in flight (unwinding across a plain `extern "C"`
/// fn is UB; the guard turns it into a defined abort). That is exactly the
/// behaviour issue #7 asks the stubs to have: fail loudly, by name, the
/// first time C actually calls one, rather than pretend to succeed.
pub(crate) fn abort_stub(symbol: &str) -> ! {
    unimplemented!(
        "{symbol} is an issue #7 abort-stub with no real implementation yet -- \
         see docs/PORTING-NOTES.md and issue #7's pre-declared module layout for \
         which later issue replaces it"
    );
}
