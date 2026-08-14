//! `go_ub_write`, `go_write_headers`, `go_sapi_flush` -- the SAPI module's
//! unbuffered-write / send-headers / flush hooks
//! (`vendor/frankenphp/frankenphp.c:1409-1410`). Real bodies for issue #12,
//! ported from `vendor/frankenphp/frankenphp.go:430-660`, over #73's
//! [`crate::context::ResponseSink`] and #106's logging facade
//! ([`super::log`]).
//!
//! # The `responseWriter == nil` case
//!
//! Upstream branches on a missing response writer in every one of these
//! three callbacks, with a different, load-bearing behaviour each time (see
//! each function's doc comment). That state is worker-bootstrap /
//! non-HTTP-context output, and it is exactly what
//! [`crate::context::RequestContext::response_sink`]`: Option<..>` being
//! `None` represents.
//!
//! This module goes one step further than upstream: every callback here also
//! tolerates **no context at all** for `thread_index` (`CONTEXT_SLOTS`
//! returning `None`), which can happen outside any request -- extension
//! `MINIT` output, `opcache.preload`, `php_module_shutdown()` on the main
//! thread. Upstream is inconsistent about this (`go_write_headers` and
//! `go_sapi_flush` check `fc == nil`; `go_ub_write` does not, and would
//! nil-pointer-panic in Go if it ever fired with no context installed). Every
//! function below folds "no context" into the same branch as "no sink" --
//! deliberately stricter, and in `go_write_headers`'s case deliberately
//! *different* from upstream's own `fc == nil` check (see that function's
//! doc comment for why unifying the two is the right call here).

use std::os::raw::{c_char, c_int, c_uchar};

use frankenrust_sys::{go_ub_write_return, zend_llist};

use crate::context::{FlushError, CONTEXT_SLOTS};

use super::log::{self, Attr, Level};

/// Mirrors PHP's `sapi_header_struct` (`main/SAPI.h`: `{ char *header;
/// size_t header_len; }`). Not one of `frankenrust-sys`'s bindgen types --
/// `crates/frankenrust-sys/build.rs`'s allowlist does not include it, and
/// this issue does not own that file (see the top-level agent instructions'
/// "stay in your lane" rule) -- so it is reproduced here by hand. Two
/// word-sized fields with no narrower member forcing tighter packing, so its
/// layout is `#[repr(C)]`-stable: a pointer followed by a `size_t`, 16 bytes,
/// 8-byte aligned on every 64-bit target this workspace builds for.
#[repr(C)]
struct SapiHeaderStruct {
    header: *mut c_char,
    header_len: usize,
}

/// Logs `buf` at INFO, the way `go_ub_write` handles output when there is no
/// real response writer to send it to (`frankenphp.go:475-485`: "probably
/// starting a worker script, log the output"). Applied uniformly to both the
/// "no sink, live context" case upstream actually has, and the "no context
/// at all" case this module additionally tolerates -- see this module's doc
/// comment.
fn log_worker_output(buf: &[u8]) {
    log::log(Level::INFO, || buf.to_vec(), Vec::new);
}

/// `frankenphp.c:1141`, inside `frankenphp_ub_write()`
/// (`sapi_module_struct.ub_write`). Ported from `frankenphp.go:430-488`.
#[unsafe(no_mangle)]
pub extern "C" fn go_ub_write(
    thread_index: usize,
    c_buf: *mut c_char,
    length: usize,
) -> go_ub_write_return {
    let buf: &[u8] = if length == 0 {
        &[]
    } else {
        // SAFETY: `c_buf`/`length` are PHP's own `str`/`str_length`
        // (`frankenphp.c:1132-1146`, `frankenphp_ub_write`), valid for reads
        // of exactly `length` bytes for the duration of this call and
        // unmodified by us. PHP strings are arbitrary bytes, so this reads
        // `u8`, never assumes UTF-8 or a NUL terminator.
        unsafe { std::slice::from_raw_parts(c_buf.cast::<u8>(), length) }
    };

    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        let Some(ctx) = ctx else {
            log_worker_output(buf);
            return go_ub_write_return {
                r0: buf.len(),
                r1: false,
            };
        };

        if ctx.is_done {
            // frankenphp.go:435-452: the request already finished (e.g. via
            // fastcgi_finish_request()), so the sink may no longer be safe
            // to write to. Discard the write, report the full length as
            // "written", and report the client's connection state as of
            // close_context() -- NOT a fresh check, which would read
            // "aborted" for virtually every post-finish write, since firing
            // the completion signal is what lets the awaiting handler
            // return and cancel the request.
            return go_ub_write_return {
                r0: buf.len(),
                r1: ctx.client_had_closed,
            };
        }

        // Evaluated to an owned value so the mutable borrow of
        // `ctx.response_sink` ends before `ctx.client_has_closed()` (which
        // needs `&ctx`) runs below.
        let write_result = ctx.response_sink.as_deref_mut().map(|sink| sink.write(buf));

        match write_result {
            Some(Ok(written)) => go_ub_write_return {
                r0: written,
                r1: ctx.client_has_closed(),
            },
            Some(Err(e)) => {
                // frankenphp.go:467-473: a write error is not fatal to the
                // request -- log it and report zero bytes written, since
                // `io::Result::Err` (unlike Go's simultaneous `(n, err)`
                // return) carries no partial count of its own.
                log::log(
                    Level::DEBUG,
                    || b"write error".to_vec(),
                    || {
                        vec![Attr {
                            key: "error",
                            value: e.to_string(),
                        }]
                    },
                );
                go_ub_write_return {
                    r0: 0,
                    r1: ctx.client_has_closed(),
                }
            }
            None => {
                log_worker_output(buf);
                go_ub_write_return {
                    r0: buf.len(),
                    r1: ctx.client_has_closed(),
                }
            }
        }
    })
}

/// Port of `splitRawHeader` (`frankenphp.go:542-569`) plus the empty-key
/// check its sole caller, `addHeader`, performs immediately afterward
/// (`frankenphp.go:530-531`). Upstream signals "no colon found" and "colon
/// at position 0" with the same empty-string sentinel, so both collapse into
/// this function's `None` rather than being reproduced as two separate
/// checks at the call site.
///
/// Returns `(key, value)`, both raw bytes -- PHP keeps ownership of `raw`,
/// this only ever borrows from it.
fn split_header(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = raw.iter().position(|&b| b == b':')?;
    if colon == 0 {
        return None;
    }

    let key = &raw[..colon];
    let mut value_start = colon + 1;
    while value_start < raw.len() && raw[value_start] == b' ' {
        value_start += 1;
    }

    Some((key, &raw[value_start..]))
}

/// `frankenphp.go:598-609`: the status PHP handed C, clamped to the range
/// Go's `net/http` accepts (`WriteHeader` panics outside `100..=999`).
/// Returns the value to actually send, and whether clamping happened, so the
/// caller can log the *discarded* raw value before it is replaced
/// (`frankenphp.go:601`: `slog.Int("status_code", goStatus)` runs before the
/// `goStatus = 500` assignment).
fn clamp_status(raw: c_int) -> (u16, bool) {
    if (100..=999).contains(&raw) {
        (raw as u16, false)
    } else {
        (500, true)
    }
}

/// `frankenphp.c:1169`, inside `frankenphp_send_headers()`. Ported from
/// `frankenphp.go:572-622`.
///
/// # Safety
///
/// `headers` must be a valid, non-null pointer to a `zend_llist` whose
/// `head`/`next` chain is well-formed and, for every node, whose `data`
/// holds an inline `sapi_header_struct` -- exactly what `sapi_headers->
/// headers` is for the duration of `frankenphp_send_headers`
/// (`frankenphp.c:1148-1174`), this function's sole caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_write_headers(
    thread_index: usize,
    status: c_int,
    headers: *mut zend_llist,
) -> bool {
    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        let Some(ctx) = ctx else {
            // Deliberately *different* from upstream's own `fc == nil`
            // check (frankenphp.go:582-585), which returns false there: see
            // this module's doc comment. No context and no sink both mean
            // "not currently producing a real HTTP response", and the
            // no-sink branch below already has a specific, correct answer
            // for that ("pretend we wrote headers so PHP still calls
            // ub_write") -- returning false here instead would just make
            // PHP treat a perfectly normal worker-bootstrap write as a
            // header-send failure.
            return true;
        };

        if ctx.is_done {
            // frankenphp.go:578-580.
            return false;
        }

        let Some(sink) = ctx.response_sink.as_deref_mut() else {
            // frankenphp.go:584-587: probably starting a worker script;
            // pretend headers were written so PHP still calls ub_write.
            return true;
        };

        // SAFETY: `headers` is `&sapi_headers->headers`
        // (`frankenphp_send_headers`, `frankenphp.c:1148-1174`), a field of
        // a struct PHP keeps alive for the whole call -- taking a field's
        // address is always non-null, so `headers` itself needs no null
        // check here (unlike a context/sink, which upstream and this port
        // both treat as legitimately absent).
        let mut current = unsafe { (*headers).head };
        while !current.is_null() {
            // SAFETY: `current` is non-null (loop guard) and, on every
            // iteration, either `(*headers).head` or a previous node's
            // `next` -- both owned by the same live `zend_llist` for the
            // duration of this call. `data`'s first `size_of::<
            // SapiHeaderStruct>()` bytes are exactly the `sapi_header_struct`
            // PHP wrote there (`frankenphp.go:589-595` does the identical
            // reinterpretation in Go: `(*C.sapi_header_struct)(unsafe.
            // Pointer(&(current.data)))`); `data` is declared `[c_char; 1]`
            // (the pre-C99 flexible-array-member trick, `docs/PORTING-NOTES.md`),
            // so its *address*, not its own 1-byte extent, is what matters --
            // C allocates each node with room for the real payload past that
            // declared byte.
            let header = unsafe { &*(&raw const (*current).data).cast::<SapiHeaderStruct>() };

            let raw: &[u8] = if header.header_len == 0 {
                &[]
            } else {
                // SAFETY: `header.header` is PHP's own header-line buffer
                // (`sapi_header_struct.header`), valid for reads of
                // `header.header_len` bytes for the duration of this call --
                // the same buffer `addHeader`/`splitRawHeader` read upstream
                // (`frankenphp.go:530`, `:542-543`).
                unsafe { std::slice::from_raw_parts(header.header.cast::<u8>(), header.header_len) }
            };

            match split_header(raw) {
                Some((name, value)) => {
                    // `ResponseSink::add_header` takes `&str`; PHP header
                    // names are arbitrary bytes with no encoding guarantee,
                    // same as any other PHP string, so a non-UTF-8 name is
                    // possible in principle even though real headers are
                    // tokens. Lossy conversion is the best available answer
                    // within the trait's signature (#73's, not this issue's
                    // to widen) -- it only ever affects a name that could not
                    // have been sent by a well-formed `header()` call anyway.
                    let name = String::from_utf8_lossy(name);
                    sink.add_header(&name, value);
                }
                None => {
                    // frankenphp.go:519-527. Raw bytes go straight into the
                    // log *message* (not an attribute -- the facade's `Attr`
                    // is `String`-valued, i.e. UTF-8, and this preserves an
                    // invalid header byte-for-byte instead of lossily
                    // collapsing it).
                    log::log(
                        Level::DEBUG,
                        || [b"invalid header: ".as_slice(), raw].concat(),
                        Vec::new,
                    );
                }
            }

            // SAFETY: same node as above; `next` is either another live
            // node or null, terminating the loop.
            current = unsafe { (*current).next };
        }

        let (effective_status, was_clamped) = clamp_status(status);
        if was_clamped {
            log::log(
                Level::WARN,
                || b"Invalid response status code".to_vec(),
                || {
                    vec![Attr {
                        key: "status_code",
                        value: status.to_string(),
                    }]
                },
            );
        }

        sink.write_status(effective_status);

        if effective_status < 200 {
            // frankenphp.go:613-619: WriteHeader does not clear the header
            // map for a 1xx on its own.
            sink.clear_headers();
        }

        true
    })
}

/// `frankenphp.c:1186`, inside `frankenphp_sapi_flush()`. Ported from
/// `frankenphp.go:624-660`.
///
/// Returns whether the client has disconnected, **not** whether the flush
/// succeeded -- C calls `php_handle_aborted_connection()` when this is true
/// (`frankenphp.c:1177-1188`).
#[unsafe(no_mangle)]
pub extern "C" fn go_sapi_flush(thread_index: usize) -> c_uchar {
    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        let Some(ctx) = ctx else {
            // frankenphp.go:630-632 (`fc == nil`), and this module's doc
            // comment -- already matches upstream's own nil check here, no
            // divergence needed.
            return false as c_uchar;
        };

        if ctx.response_sink.is_none() {
            // frankenphp.go:634-636: nothing to flush.
            return false as c_uchar;
        }

        if ctx.client_has_closed() && !ctx.is_done {
            // frankenphp.go:638-640: skip the flush attempt entirely.
            return true as c_uchar;
        }

        let sink = ctx
            .response_sink
            .as_deref_mut()
            .expect("checked Some above");

        match sink.flush() {
            Ok(()) => {}
            Err(FlushError::NotAFlusher) => {
                log::log(
                    Level::WARN,
                    || {
                        b"the current responseWriter is not a flusher, if you are not using \
                          a custom build, please report this issue"
                            .to_vec()
                    },
                    Vec::new,
                );
            }
            Err(FlushError::Io(e)) => {
                log::log(
                    Level::DEBUG,
                    || b"flush error".to_vec(),
                    || {
                        vec![Attr {
                            key: "error",
                            value: e.to_string(),
                        }]
                    },
                );
            }
        }

        false as c_uchar
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::ptr;
    use std::sync::{Arc, Mutex};

    use frankenrust_sys::{_zend_llist_element, zend_llist};

    use super::*;
    use crate::context::{CompletionSignal, Headers, Request, RequestContext, ResponseSink};

    // See misc.rs's `tests` module doc comment for why these must be small
    // and distinct from every other file's indices into the process-global
    // `CONTEXT_SLOTS`: misc.rs uses 1-4, servervars.rs uses 60-76. This
    // file's own indices, and input.rs's, live in disjoint ranges below.
    const IDX_UB_WRITE_NO_CONTEXT: usize = 100;
    const IDX_UB_WRITE_NO_SINK: usize = 101;
    const IDX_UB_WRITE_WRITES: usize = 102;
    const IDX_UB_WRITE_SHORT: usize = 103;
    const IDX_UB_WRITE_DONE_CACHED: usize = 104;
    const IDX_WRITE_HEADERS_NO_CONTEXT: usize = 105;
    const IDX_WRITE_HEADERS_NO_SINK: usize = 106;
    const IDX_WRITE_HEADERS_DONE: usize = 107;
    const IDX_WRITE_HEADERS_FORWARDS: usize = 108;
    const IDX_FLUSH_NO_CONTEXT: usize = 109;
    const IDX_FLUSH_NO_SINK: usize = 110;
    const IDX_FLUSH_CALLS: usize = 111;
    const IDX_FLUSH_CLOSED_NOT_DONE: usize = 112;
    const IDX_FLUSH_CLOSED_DONE: usize = 113;

    fn fresh_context() -> RequestContext {
        RequestContext::new(String::new(), None, None, CompletionSignal::none())
    }

    fn context_with_request() -> RequestContext {
        RequestContext::new(
            String::new(),
            None,
            Some(Request::new("GET", b"/".to_vec())),
            CompletionSignal::none(),
        )
    }

    /// What a [`FakeSink`] recorded, readable after the callback under test
    /// returns -- a `Box<dyn ResponseSink>` moved into a `RequestContext`
    /// can't be downcast back out through the trait, so this is shared via
    /// `Arc<Mutex<_>>` rather than owned by the sink itself.
    #[derive(Default)]
    struct FakeSinkState {
        writes: Vec<Vec<u8>>,
        header_adds: Vec<(String, Vec<u8>)>,
        headers: Headers,
        statuses: Vec<u16>,
        flush_calls: usize,
    }

    struct FakeSink {
        state: Arc<Mutex<FakeSinkState>>,
        next_write: Option<io::Result<usize>>,
        next_flush: Option<Result<(), FlushError>>,
    }

    impl FakeSink {
        fn new() -> (Self, Arc<Mutex<FakeSinkState>>) {
            let state = Arc::new(Mutex::new(FakeSinkState::default()));
            (
                Self {
                    state: Arc::clone(&state),
                    next_write: None,
                    next_flush: None,
                },
                state,
            )
        }
    }

    impl ResponseSink for FakeSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.state.lock().unwrap().writes.push(buf.to_vec());
            self.next_write.take().unwrap_or(Ok(buf.len()))
        }

        fn add_header(&mut self, name: &str, value: &[u8]) {
            let mut state = self.state.lock().unwrap();
            state.header_adds.push((name.to_string(), value.to_vec()));
            state.headers.insert(name, value.to_vec());
        }

        fn clear_headers(&mut self) {
            self.state.lock().unwrap().headers.clear();
        }

        fn write_status(&mut self, status: u16) {
            self.state.lock().unwrap().statuses.push(status);
        }

        fn flush(&mut self) -> Result<(), FlushError> {
            self.state.lock().unwrap().flush_calls += 1;
            self.next_flush.take().unwrap_or(Ok(()))
        }
    }

    // -------------------------------------------------------------------
    // split_header
    // -------------------------------------------------------------------

    #[test]
    fn split_header_with_no_colon_is_none() {
        assert_eq!(split_header(b"NoColonHere"), None);
    }

    #[test]
    fn split_header_with_leading_colon_is_none() {
        // Empty key: upstream's addHeader treats this identically to "no
        // colon at all" (frankenphp.go:530-531).
        assert_eq!(split_header(b":value"), None);
    }

    #[test]
    fn split_header_skips_leading_spaces_in_the_value() {
        assert_eq!(
            split_header(b"X-Foo:   value"),
            Some((b"X-Foo".as_slice(), b"value".as_slice()))
        );
    }

    #[test]
    fn split_header_only_skips_spaces_not_other_whitespace() {
        assert_eq!(
            split_header(b"X-Foo:\tvalue"),
            Some((b"X-Foo".as_slice(), b"\tvalue".as_slice()))
        );
    }

    #[test]
    fn split_header_empty_value() {
        assert_eq!(
            split_header(b"X-Foo:"),
            Some((b"X-Foo".as_slice(), b"".as_slice()))
        );
        assert_eq!(
            split_header(b"X-Foo:   "),
            Some((b"X-Foo".as_slice(), b"".as_slice())),
            "trailing spaces after the colon must also collapse to an empty value"
        );
    }

    #[test]
    fn split_header_value_may_be_non_utf8() {
        let raw: &[u8] = b"X-Foo: \xff\xfe";
        assert_eq!(
            split_header(raw),
            Some((b"X-Foo".as_slice(), b"\xff\xfe".as_slice())),
            "a non-UTF-8 value must survive unchanged, not be replaced or rejected"
        );
    }

    #[test]
    fn split_header_value_containing_a_colon_splits_at_the_first_one_only() {
        assert_eq!(
            split_header(b"X-Foo: a:b"),
            Some((b"X-Foo".as_slice(), b"a:b".as_slice()))
        );
    }

    // -------------------------------------------------------------------
    // clamp_status
    // -------------------------------------------------------------------

    #[test]
    fn clamp_status_below_100_clamps_to_500() {
        assert_eq!(clamp_status(99), (500, true));
    }

    #[test]
    fn clamp_status_100_is_in_range() {
        assert_eq!(clamp_status(100), (100, false));
    }

    #[test]
    fn clamp_status_999_is_in_range() {
        assert_eq!(clamp_status(999), (999, false));
    }

    #[test]
    fn clamp_status_above_999_clamps_to_500() {
        assert_eq!(clamp_status(1000), (500, true));
    }

    // -------------------------------------------------------------------
    // go_ub_write
    // -------------------------------------------------------------------

    #[test]
    fn go_ub_write_with_no_context_does_not_abort_and_reports_the_full_length() {
        let idx = IDX_UB_WRITE_NO_CONTEXT;
        let mut payload = b"hello".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());
        assert_eq!(result.r0, payload.len());
        assert!(!result.r1);
    }

    #[test]
    fn go_ub_write_with_no_sink_logs_and_reports_the_full_length() {
        let idx = IDX_UB_WRITE_NO_SINK;
        CONTEXT_SLOTS.set(idx, fresh_context());

        let mut payload = b"worker output".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());

        CONTEXT_SLOTS.clear(idx);

        assert_eq!(result.r0, payload.len());
        assert!(!result.r1);
    }

    #[test]
    fn go_ub_write_delivers_bytes_to_the_sink_and_returns_its_count() {
        let idx = IDX_UB_WRITE_WRITES;
        let mut ctx = context_with_request();
        let (sink, state) = FakeSink::new();
        ctx.response_sink = Some(Box::new(sink));
        CONTEXT_SLOTS.set(idx, ctx);

        let mut payload = b"hello world".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());

        CONTEXT_SLOTS.clear(idx);

        assert_eq!(result.r0, payload.len());
        assert!(!result.r1, "a fresh context's request is never cancelled");
        assert_eq!(state.lock().unwrap().writes, vec![payload]);
    }

    #[test]
    fn go_ub_write_reports_a_short_write() {
        let idx = IDX_UB_WRITE_SHORT;
        let mut ctx = context_with_request();
        let (mut sink, state) = FakeSink::new();
        sink.next_write = Some(Ok(3));
        ctx.response_sink = Some(Box::new(sink));
        CONTEXT_SLOTS.set(idx, ctx);

        let mut payload = b"hello world".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());

        CONTEXT_SLOTS.clear(idx);

        assert_eq!(
            result.r0, 3,
            "a short write from the sink must be reported as-is"
        );
        assert_eq!(state.lock().unwrap().writes, vec![payload]);
    }

    #[test]
    fn go_ub_write_after_close_context_discards_the_write_and_reports_the_cached_snapshot() {
        // The finish-request.php regression (vendor/frankenphp/testdata/
        // finish-request.php, frankenphp_test.go:184-195): a context whose
        // *live* cancellation flag says "closed" but whose *cached*
        // client_had_closed snapshot (taken at close_context() time) says
        // "not closed" must report the cached value, not a fresh check.
        let idx = IDX_UB_WRITE_DONE_CACHED;
        let request = Request::new("GET", b"/".to_vec());
        let cancelled = request.cancelled.clone();
        let mut ctx =
            RequestContext::new(String::new(), None, Some(request), CompletionSignal::none());
        let (sink, state) = FakeSink::new();
        ctx.response_sink = Some(Box::new(sink));
        ctx.close_context();
        assert!(
            !ctx.client_had_closed,
            "close_context() must have snapshotted 'not closed' before this test flips it"
        );
        // Now simulate the client disconnecting *after* the request
        // finished -- exactly what a normal fastcgi_finish_request()
        // followed by continued script execution looks like.
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            ctx.client_has_closed(),
            "the live flag must now disagree with the cached snapshot"
        );
        CONTEXT_SLOTS.set(idx, ctx);

        let mut payload = b"still writing after finish".to_vec();
        let result = go_ub_write(idx, payload.as_mut_ptr().cast(), payload.len());

        let is_done_after = CONTEXT_SLOTS.with_context_mut(idx, |ctx| ctx.unwrap().is_done);
        CONTEXT_SLOTS.clear(idx);

        assert!(
            is_done_after,
            "close_context() must have marked the context done"
        );
        assert!(
            state.lock().unwrap().writes.is_empty(),
            "a post-finish write must be discarded, never reach the sink"
        );
        assert_eq!(
            result.r0,
            payload.len(),
            "a post-finish write must report the full length, not the (discarded) sink result"
        );
        assert!(
            !result.r1,
            "must report the CACHED client_had_closed (false), not the live, now-true flag"
        );
    }

    // -------------------------------------------------------------------
    // go_write_headers
    // -------------------------------------------------------------------

    /// One owned `zend_llist_element` node carrying an inline
    /// `sapi_header_struct` payload, built the way `zend_llist_add_element`
    /// (`Zend/zend_llist.c`) actually allocates one: `data`'s declared size
    /// (`[c_char; 1]`) is not the element's real size, so a Rust value of
    /// type `_zend_llist_element` would only have room for one byte of
    /// payload. Backed by `Vec<u64>`, not `Vec<u8>`, so the allocation is
    /// 8-byte aligned -- required to read the embedded `sapi_header_struct`
    /// (a pointer and a `usize`) without inducing undefined behaviour, the
    /// same property `go_write_headers` itself relies on
    /// (`SapiHeaderStruct`'s doc comment).
    struct HeaderNode {
        _text: Vec<u8>,
        _storage: Vec<u64>,
    }

    fn build_header_node(raw: &[u8]) -> (HeaderNode, *mut _zend_llist_element) {
        let text = raw.to_vec();
        let data_offset = std::mem::offset_of!(_zend_llist_element, data);
        let payload_size = std::mem::size_of::<SapiHeaderStruct>();
        let total = data_offset + payload_size;
        let words = total.div_ceil(std::mem::size_of::<u64>());
        let mut storage = vec![0u64; words];

        let base = storage.as_mut_ptr().cast::<u8>();
        // SAFETY: `base` names `words * 8 >= total` freshly allocated,
        // writable bytes, uniquely owned by `storage`, and 8-byte aligned (a
        // `Vec<u64>`'s allocation is aligned to `align_of::<u64>() == 8`,
        // which is also `align_of::<SapiHeaderStruct>()` and
        // `align_of::<*mut _zend_llist_element>()`). The three writes below
        // target disjoint, in-bounds offsets (`next`/`prev` at 0/8 --
        // `_zend_llist_element`'s own declared layout -- and the header
        // struct at `data_offset`), so this reproduces one C
        // `zend_llist_element` node without ever materialising a Rust value
        // of that type.
        unsafe {
            base.cast::<*mut _zend_llist_element>()
                .write(ptr::null_mut());
            base.add(8)
                .cast::<*mut _zend_llist_element>()
                .write(ptr::null_mut());
            base.add(data_offset)
                .cast::<SapiHeaderStruct>()
                .write(SapiHeaderStruct {
                    header: text.as_ptr().cast_mut().cast::<c_char>(),
                    header_len: text.len(),
                });
        }

        let node_ptr = base.cast::<_zend_llist_element>();
        (
            HeaderNode {
                _text: text,
                _storage: storage,
            },
            node_ptr,
        )
    }

    fn build_llist(raw_headers: &[&[u8]]) -> (Vec<HeaderNode>, zend_llist) {
        let mut nodes = Vec::new();
        let mut ptrs = Vec::new();
        for raw in raw_headers {
            let (node, ptr) = build_header_node(raw);
            nodes.push(node);
            ptrs.push(ptr);
        }

        for i in 0..ptrs.len() {
            let prev = if i == 0 { ptr::null_mut() } else { ptrs[i - 1] };
            let next = if i + 1 < ptrs.len() {
                ptrs[i + 1]
            } else {
                ptr::null_mut()
            };
            // SAFETY: every pointer in `ptrs` names a live node owned by
            // `nodes`, which the caller keeps alive at least as long as
            // `list` -- writing `next`/`prev` matches
            // `_zend_llist_element`'s declared field layout.
            unsafe {
                (*ptrs[i]).prev = prev;
                (*ptrs[i]).next = next;
            }
        }

        let list = zend_llist {
            head: ptrs.first().copied().unwrap_or(ptr::null_mut()),
            tail: ptrs.last().copied().unwrap_or(ptr::null_mut()),
            count: ptrs.len(),
            ..zend_llist::default()
        };

        (nodes, list)
    }

    #[test]
    fn go_write_headers_with_no_context_returns_true() {
        let idx = IDX_WRITE_HEADERS_NO_CONTEXT;
        let (_nodes, mut list) = build_llist(&[]);
        // SAFETY: `list` is a well-formed, live `zend_llist` built by
        // `build_llist` above, kept alive by `_nodes` for this call.
        assert!(unsafe { go_write_headers(idx, 200, &mut list) });
    }

    #[test]
    fn go_write_headers_with_no_sink_returns_true_and_does_not_walk_headers() {
        let idx = IDX_WRITE_HEADERS_NO_SINK;
        CONTEXT_SLOTS.set(idx, fresh_context());
        let (_nodes, mut list) = build_llist(&[b"X-Foo: bar"]);

        // SAFETY: see the identical justification above.
        let result = unsafe { go_write_headers(idx, 200, &mut list) };
        CONTEXT_SLOTS.clear(idx);

        assert!(result);
    }

    #[test]
    fn go_write_headers_returns_false_when_the_request_is_already_done() {
        let idx = IDX_WRITE_HEADERS_DONE;
        let mut ctx = context_with_request();
        ctx.response_sink = Some(Box::new(FakeSink::new().0));
        ctx.close_context();
        CONTEXT_SLOTS.set(idx, ctx);

        let (_nodes, mut list) = build_llist(&[]);
        // SAFETY: see the identical justification above.
        let result = unsafe { go_write_headers(idx, 200, &mut list) };
        CONTEXT_SLOTS.clear(idx);

        assert!(!result);
    }

    #[test]
    fn go_write_headers_forwards_every_header_sets_status_and_clears_after_a_1xx() {
        // The early-hints sequence (vendor/frankenphp/testdata/early-hints.php,
        // frankenphp_test.go:604-605): go_write_headers runs twice, once for
        // a 103 and once for the final 200, and upstream clears the sink's
        // header map in between so the 103's headers don't leak into the
        // final response.
        let idx = IDX_WRITE_HEADERS_FORWARDS;
        let mut ctx = context_with_request();
        let (sink, state) = FakeSink::new();
        ctx.response_sink = Some(Box::new(sink));
        CONTEXT_SLOTS.set(idx, ctx);

        let (_nodes, mut list) =
            build_llist(&[b"Link: </style.css>; rel=preload; as=style", b"Request: 7"]);
        // SAFETY: `list` is a well-formed, live `zend_llist` built by
        // `build_llist` above, kept alive by `_nodes` for this call.
        assert!(unsafe { go_write_headers(idx, 103, &mut list) });

        {
            let state = state.lock().unwrap();
            assert_eq!(
                state.header_adds,
                vec![
                    (
                        "Link".to_string(),
                        b"</style.css>; rel=preload; as=style".to_vec()
                    ),
                    ("Request".to_string(), b"7".to_vec()),
                ],
                "every element of the multi-node llist must reach the sink"
            );
            assert_eq!(
                state.headers.get_first("Link"),
                None,
                "a 1xx status must clear the sink's header map afterward"
            );
            assert_eq!(state.statuses, vec![103]);
        }

        let (_nodes2, mut list2) = build_llist(&[b"Request: 7"]);
        // SAFETY: see the identical justification above.
        assert!(unsafe { go_write_headers(idx, 200, &mut list2) });

        CONTEXT_SLOTS.clear(idx);

        let state = state.lock().unwrap();
        assert_eq!(state.statuses, vec![103, 200]);
        assert_eq!(
            state.headers.get_all("Request").map(<[Vec<u8>]>::len),
            Some(1),
            "Request must not accumulate a second value across the cleared 1xx"
        );
    }

    // -------------------------------------------------------------------
    // go_sapi_flush
    // -------------------------------------------------------------------

    #[test]
    fn go_sapi_flush_with_no_context_returns_false() {
        assert_eq!(go_sapi_flush(IDX_FLUSH_NO_CONTEXT), 0);
    }

    #[test]
    fn go_sapi_flush_with_no_sink_returns_false() {
        let idx = IDX_FLUSH_NO_SINK;
        CONTEXT_SLOTS.set(idx, fresh_context());
        let result = go_sapi_flush(idx);
        CONTEXT_SLOTS.clear(idx);
        assert_eq!(result, 0);
    }

    #[test]
    fn go_sapi_flush_calls_the_sinks_flush_and_returns_false_on_success() {
        let idx = IDX_FLUSH_CALLS;
        let mut ctx = context_with_request();
        ctx.response_sink = Some(Box::new(FakeSink::new().0));
        CONTEXT_SLOTS.set(idx, ctx);

        let result = go_sapi_flush(idx);
        CONTEXT_SLOTS.clear(idx);

        assert_eq!(result, 0);
    }

    #[test]
    fn go_sapi_flush_returns_true_only_when_closed_and_not_done() {
        let idx = IDX_FLUSH_CLOSED_NOT_DONE;
        let request = Request::new("GET", b"/".to_vec());
        request
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut ctx =
            RequestContext::new(String::new(), None, Some(request), CompletionSignal::none());
        ctx.response_sink = Some(Box::new(FakeSink::new().0));
        CONTEXT_SLOTS.set(idx, ctx);

        let result = go_sapi_flush(idx);
        CONTEXT_SLOTS.clear(idx);

        assert_eq!(
            result, 1,
            "closed and not done must report true (client disconnected)"
        );
    }

    #[test]
    fn go_sapi_flush_returns_false_when_closed_but_already_done() {
        let idx = IDX_FLUSH_CLOSED_DONE;
        let request = Request::new("GET", b"/".to_vec());
        request
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut ctx =
            RequestContext::new(String::new(), None, Some(request), CompletionSignal::none());
        ctx.response_sink = Some(Box::new(FakeSink::new().0));
        ctx.close_context();
        CONTEXT_SLOTS.set(idx, ctx);

        let result = go_sapi_flush(idx);
        CONTEXT_SLOTS.clear(idx);

        assert_eq!(result, 0, "a done request must not report a fresh abort");
    }
}
