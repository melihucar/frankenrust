//! Safe layer: everything that runs *on* a PHP pthread. For issue #7 this
//! crate contains only the 26 `go_*` callback symbols `frankenphp.c` links
//! against, each an abort-stub (see `callbacks` and `docs/PORTING-NOTES.md`).
//! The thread state machine, per-thread lifecycle and request context
//! (`state.rs`, `thread.rs`, `context.rs`) are out of scope here -- #8, #10
//! and #11 add them.

pub mod callbacks;
pub mod state;
