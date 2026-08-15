//! Async side: the hyper HTTP/1.1 listener and the bridge that hands a
//! request to a PHP thread and awaits its result (`docs/ARCHITECTURE.md`).
//!
//! Owns everything downstream of accepting a TCP connection: translating a
//! hyper request into `frankenrust_core::context::Request`
//! ([`request`]), the buffered [`frankenrust_core::context::ResponseSink`]
//! this server writes through ([`sink`]), the single seam that turns
//! accumulated response state into a `hyper::Response` ([`response`]), and
//! the accept loop plus PHP-thread-pool bootstrap ([`server`]). The
//! dispatch/drain/handler state machine this crate hands requests to lives
//! in `frankenrust_core::thread_regular` -- this crate never touches PHP
//! state directly, only the socket and the request/response buffers
//! (`docs/PORTING-NOTES.md`'s async<->pthread boundary rules).
pub mod request;
pub mod response;
pub mod server;
pub mod sink;

pub use server::{bind, run, ServerConfig};
