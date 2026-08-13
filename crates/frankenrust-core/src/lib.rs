//! Safe layer: everything that runs *on* a PHP pthread. For issue #7 this
//! crate contains only the 26 `go_*` callback symbols `frankenphp.c` links
//! against, each an abort-stub (see `callbacks` and `docs/PORTING-NOTES.md`).
//! The thread state machine and per-thread lifecycle (`state.rs`, `thread.rs`)
//! are out of scope here -- #8 and #10 add them. `context.rs` (the request
//! context and its per-thread slot table) is added by #79.

pub mod callbacks;
pub mod context;
