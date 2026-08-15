//! The [`ResponseSink`] implementation that backs
//! `frankenrust_core::context::RequestContext::response_sink` for every
//! request this server dispatches.
//!
//! # Why buffered rather than streamed
//!
//! PHP calls [`ResponseSink`]'s methods synchronously, from the PHP pthread,
//! at any point during script execution -- but hyper needs a complete
//! `Response<Full<Bytes>>` handed back from the service function, and the
//! only signal the async side has that a script is done producing output is
//! the completion oneshot [`RequestContext::close_context`] fires
//! (`docs/ARCHITECTURE.md`'s async<->pthread boundary section). So this sink
//! buffers status, headers and body into a plain, `Mutex`-guarded struct
//! that the async task reads out **once**, after that oneshot resolves --
//! never before, and never again after (a write ordering `context.rs`
//! itself guarantees: `close_context` runs strictly after the script's own
//! output, and `go_ub_write` already discards any write that arrives once
//! `ctx.is_done`, matching upstream's `frankenphp.go:435-452`). That is
//! "Response bytes cross via ... a mutex-guarded buffer", the second option
//! this issue's body names -- not a stub, a deliberate simplification this
//! issue's "seam" note anticipates #159 replacing wholesale with a real
//! streaming, byte-exact writer.
//!
//! Filed as #170: this buffering makes `flush()`/`ob_flush()` and every
//! streaming pattern built on them functionally dead (nothing reaches the
//! client until the script ends), and holds an unbounded response fully
//! resident in memory for the life of a request. Both are real costs, not
//! hypothetical -- #170 has a measured repro -- and out of this issue's
//! scope to fix (`docs/ARCHITECTURE.md`'s crate boundary keeps tokio out of
//! `frankenrust-core`, so a real streaming sink needs its own design, not a
//! patch to this one).
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use frankenrust_core::context::{FlushError, Headers, ResponseSink};

/// What accumulates for one request while its script runs, and what
/// [`crate::response::build_response`] reads once the request is done.
#[derive(Default)]
pub struct SharedResponseBuffer {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
}

pub struct BufferedResponseSink {
    shared: Arc<Mutex<SharedResponseBuffer>>,
}

impl BufferedResponseSink {
    pub fn new(shared: Arc<Mutex<SharedResponseBuffer>>) -> Self {
        Self { shared }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SharedResponseBuffer> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ResponseSink for BufferedResponseSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().body.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn add_header(&mut self, name: &str, value: &[u8]) {
        self.lock().headers.insert(name, value.to_vec());
    }

    fn clear_headers(&mut self) {
        self.lock().headers.clear();
    }

    fn write_status(&mut self, status: u16) {
        self.lock().status = status;
    }

    fn flush(&mut self) -> Result<(), FlushError> {
        // Nothing can be flushed early out of a buffer that is only ever
        // read once, at the end -- but that is not the same claim as "this
        // sink cannot flush at all" (`FlushError::NotAFlusher`), which would
        // make `go_sapi_flush` warn on every `flush()` call a hello-world
        // script never makes but a real one might. The whole response is
        // sent once the request completes regardless, so reporting success
        // here is accurate, not a fabricated one -- #159's streaming writer
        // is what gives an early `flush()` an observable effect.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_headers_and_status_accumulate_and_are_readable_back() {
        let shared = Arc::new(Mutex::new(SharedResponseBuffer::default()));
        let mut sink = BufferedResponseSink::new(Arc::clone(&shared));

        sink.write_status(201);
        sink.add_header("X-Foo", b"bar");
        sink.add_header("X-Foo", b"baz");
        assert_eq!(sink.write(b"hello ").unwrap(), 6);
        assert_eq!(sink.write(b"world").unwrap(), 5);
        assert!(sink.flush().is_ok());

        let buffer = shared.lock().unwrap();
        assert_eq!(buffer.status, 201);
        assert_eq!(
            buffer.headers.get_all("X-Foo"),
            Some([b"bar".to_vec(), b"baz".to_vec()].as_slice())
        );
        assert_eq!(buffer.body, b"hello world");
    }

    #[test]
    fn clear_headers_empties_the_map_but_not_the_body() {
        let shared = Arc::new(Mutex::new(SharedResponseBuffer::default()));
        let mut sink = BufferedResponseSink::new(Arc::clone(&shared));

        sink.add_header("X-Foo", b"bar");
        sink.write(b"partial").unwrap();
        sink.clear_headers();

        let buffer = shared.lock().unwrap();
        assert_eq!(buffer.headers.get_first("X-Foo"), None);
        assert_eq!(buffer.body, b"partial");
    }
}
