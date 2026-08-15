//! Safe layer: everything that runs *on* a PHP pthread. For issue #7 this
//! crate contains only the 26 `go_*` callback symbols `frankenphp.c` links
//! against, each an abort-stub (see `callbacks` and `docs/PORTING-NOTES.md`).
//! The state machine, request context, and PHP-thread lifecycle are the safe
//! Rust side of the C SAPI bridge.

pub mod callbacks;
pub mod cgi;
pub mod context;
pub mod state;
pub mod thread;
pub mod thread_inactive;
pub mod thread_regular;
