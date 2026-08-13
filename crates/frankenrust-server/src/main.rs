//! Async side: the hyper HTTP/1.1 listener and the bridge that hands a
//! request to a PHP thread and awaits its result (`docs/ARCHITECTURE.md`).
//!
//! Empty placeholder: issue #7 only wires up the workspace and the FFI
//! foundation (`frankenrust-sys`, `frankenrust-core`'s callback abort-stubs).
//! The thread pool, the HTTP server and the request context are explicitly
//! out of scope for that issue; later issues build this crate out.

fn main() {
    eprintln!(
        "frankenrust-server is not implemented yet -- issue #7 only lays the FFI foundation \
         (frankenrust-sys, frankenrust-core's callback abort-stubs). See docs/ARCHITECTURE.md."
    );
    std::process::exit(1);
}
