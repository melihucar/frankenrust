//! `go_read_post`, `go_read_cookies`, `go_apache_request_headers` -- request
//! body/cookie/header hooks (`vendor/frankenphp/frankenphp.c:1191-1196`,
//! `:762-776`). Real bodies for issue #12, ported from
//! `vendor/frankenphp/frankenphp.go:662-720`, `:490-527`, over #73's
//! [`crate::context::RequestBody`]/[`crate::context::CONTEXT_SLOTS`] and
//! #106's logging facade ([`super::log`]).
//!
//! Every callback here tolerates both "no request context at all" for
//! `thread_index` and "context present but not handling a real HTTP request"
//! (no sink installed) -- see `output.rs`'s module doc comment for why the
//! two are folded together. Request-body timeouts
//! (`frankenphp.go:670-681`) are out of scope: #73's `RequestBody` has no
//! deadline API, and this issue does not add one.

use std::os::raw::c_char;
use std::ptr;

use frankenrust_sys::{go_apache_request_headers_return, go_string, malloc};

use crate::context::{RequestArena, CONTEXT_SLOTS};

use super::log::{self, Level};

/// `frankenphp.c:1192`, inside `frankenphp_read_post()`
/// (`sapi_module_struct.read_post`). Ported from `frankenphp.go:662-701`,
/// minus the request-body-timeout deadline plumbing (out of scope: see this
/// module's doc comment).
#[unsafe(no_mangle)]
pub extern "C" fn go_read_post(
    thread_index: usize,
    c_buf: *mut c_char,
    count_bytes: usize,
) -> usize {
    if count_bytes == 0 {
        return 0;
    }

    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        let has_sink = ctx.as_ref().is_some_and(|ctx| ctx.response_sink.is_some());
        if !has_sink {
            // frankenphp.go:666-668, and (for a missing context) this
            // crate's stricter-than-upstream nil tolerance -- see
            // output.rs's module doc comment.
            return 0;
        }
        let ctx = ctx.expect("has_sink implies Some");

        // SAFETY: `c_buf`/`count_bytes` are PHP's own `buffer`/`count_bytes`
        // (`frankenphp.c:1191-1193`, `frankenphp_read_post`), writable for
        // exactly `count_bytes` bytes for the duration of this call --
        // mirroring `fc.request.Body.Read` writing into the identical
        // `unsafe.Slice` upstream (`frankenphp.go:683`). `count_bytes != 0`
        // is checked above.
        let buf = unsafe { std::slice::from_raw_parts_mut(c_buf.cast::<u8>(), count_bytes) };

        ctx.request
            .as_mut()
            .map_or(0, |request| request.body.fill(buf))
    })
}

/// Joins `values` with `sep` between elements -- the shared shape behind two
/// *different* separators this module needs for two different headers:
/// `", "` for every ordinary request header (`frankenphp.go:512`,
/// consumed by [`go_apache_request_headers`]) and `"; "` specifically for
/// `Cookie` (`frankenphp.go:710`, [`go_read_cookies`]). Kept as one
/// parameterised helper rather than two copies, but every call site names
/// its own separator explicitly so the two rules stay visibly distinct.
fn join_values(values: &[Vec<u8>], sep: &[u8]) -> Vec<u8> {
    let mut joined = Vec::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            joined.extend_from_slice(sep);
        }
        joined.extend_from_slice(value);
    }
    joined
}

/// Copies `bytes` into a fresh, NUL-terminated `libc::malloc` allocation --
/// the C-ownership half of `docs/PORTING-NOTES.md`'s `C.CString` mapping.
/// Ownership crosses to C, which `free()`s it
/// (`frankenphp_free_request_context`, `frankenphp.c:362-365`); `CString::
/// into_raw` would be wrong here, since it allocates from Rust's global
/// allocator, not libc's `malloc`, and handing that to C's `free()` is
/// undefined behaviour.
///
/// Returns NULL on allocation failure, or if `bytes.len() + 1` does not fit
/// `malloc`'s size parameter -- [`go_read_cookies`], this function's only
/// caller, already treats NULL as "no cookies", so this reuses that path
/// rather than returning a truncated allocation.
fn malloc_c_string(bytes: &[u8]) -> *mut c_char {
    let Ok(alloc_size) = std::os::raw::c_ulong::try_from(bytes.len() + 1) else {
        return ptr::null_mut();
    };

    // SAFETY: `malloc` is libc's allocator (`frankenrust-sys`'s bindgen
    // binding of the system `malloc`); `alloc_size` is nonzero (it includes
    // the trailing NUL byte this function appends below).
    let allocation = unsafe { malloc(alloc_size) }.cast::<c_char>();
    if allocation.is_null() {
        return allocation;
    }

    // SAFETY: `allocation` names `bytes.len() + 1` freshly malloc'd,
    // writable bytes with no other live reference (just allocated, checked
    // non-null above). `bytes` is a disjoint, valid source of `bytes.len()`
    // bytes -- a fresh heap allocation cannot overlap it. The trailing write
    // is in-bounds: `allocation` has room for exactly `bytes.len() + 1`
    // bytes, and this writes to the last of them.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), allocation, bytes.len());
        *allocation.add(bytes.len()) = 0;
    }
    allocation
}

/// `frankenphp.c:1196`, inside `frankenphp_read_cookies()`
/// (`sapi_module_struct.read_cookies`). Ported from `frankenphp.go:703-720`.
#[unsafe(no_mangle)]
pub extern "C" fn go_read_cookies(thread_index: usize) -> *mut c_char {
    CONTEXT_SLOTS.with_context(thread_index, |ctx| {
        // frankenphp.go:705-707 checks only `request == nil`; a missing
        // context folds into the same "nothing to read" answer (this
        // crate's stricter-than-upstream nil tolerance, output.rs's module
        // doc comment).
        let Some(request) = ctx.and_then(|ctx| ctx.request.as_ref()) else {
            return ptr::null_mut();
        };

        let values = request.headers.get_all("Cookie").unwrap_or(&[]);
        let mut joined = join_values(values, b"; ");

        // frankenphp.go:711-714: the emptiness check runs on the joined
        // string *before* NUL-stripping -- so a Cookie header consisting
        // entirely of NUL bytes is not "empty" here and does not return
        // NULL (it becomes an empty, non-NULL C string after stripping).
        if joined.is_empty() {
            return ptr::null_mut();
        }

        joined.retain(|&b| b != 0);

        malloc_c_string(&joined)
    })
}

/// Reserves room for `count` [`go_string`]s inside `arena`, at a genuinely
/// 8-byte-aligned address. `RequestArena::alloc` copies its input into a
/// `Vec<u8>`-backed allocation (`context.rs`) with no alignment guarantee
/// beyond 1, but `go_string` embeds a pointer
/// (`align_of::<go_string>() == 8` on every 64-bit target this workspace
/// builds for), and the C caller indexes the returned array with ordinary
/// `headers.r0[i]` (`frankenphp.c:770-775`), which assumes natural
/// alignment -- exactly what Go's own `make([]C.go_string, ...)` guarantees
/// upstream (`frankenphp.go:506`). This reproduces that guarantee by
/// over-allocating `align_of::<go_string>() - 1` extra bytes and rounding
/// the arena's returned pointer up to the next multiple of that alignment;
/// the extra bytes are unused tail padding of the arena's own dedicated
/// buffer for this one allocation (`RequestArena::alloc`'s doc comment: each
/// call gets its own `Box<[u8]>`, never shared with another allocation), so
/// nothing else is affected by consuming them.
fn alloc_go_strings(arena: &mut RequestArena, count: usize) -> *mut go_string {
    let align = std::mem::align_of::<go_string>();
    let size = count * std::mem::size_of::<go_string>();
    let scratch = vec![0u8; size + align - 1];

    let base = arena.alloc(&scratch);
    let addr = base as usize;
    let aligned_addr = addr.next_multiple_of(align);
    let offset = aligned_addr - addr;

    // SAFETY: `base` is valid for `scratch.len()` writable bytes for the
    // rest of the request (`RequestArena::alloc`'s own SAFETY comment).
    // `offset < align` (rounding up to the next multiple of a power-of-two
    // alignment never advances by a full `align`), and `scratch.len() ==
    // size + align - 1 >= offset + size`, so `base.add(offset)` stays
    // in-bounds of that one allocation with `size` bytes still free after
    // it for the caller's `go_string` writes.
    unsafe { base.add(offset) }.cast::<go_string>()
}

/// `frankenphp.c:766`, inside `PHP_FUNCTION(frankenphp_request_headers)` --
/// backs userland `apache_request_headers()` / `getallheaders()`. Ported
/// from `frankenphp.go:490-527`.
#[unsafe(no_mangle)]
pub extern "C" fn go_apache_request_headers(
    thread_index: usize,
) -> go_apache_request_headers_return {
    CONTEXT_SLOTS.with_context_mut(thread_index, |ctx| {
        let Some(ctx) = ctx else {
            log_non_http_context();
            return go_apache_request_headers_return {
                r0: ptr::null_mut(),
                r1: 0,
            };
        };

        if ctx.response_sink.is_none() {
            log_non_http_context();
            return go_apache_request_headers_return {
                r0: ptr::null_mut(),
                r1: 0,
            };
        }

        let Some(request) = ctx.request.as_ref() else {
            log_non_http_context();
            return go_apache_request_headers_return {
                r0: ptr::null_mut(),
                r1: 0,
            };
        };

        // Collected as owned bytes first, and only then written into the
        // arena: `request` borrows `ctx.request`, and the write loop below
        // needs `&mut ctx.arena` -- disjoint fields of the same `ctx`, but
        // keeping the two phases separate avoids relying on that borrow
        // ever being live across the loop.
        let pairs: Vec<(String, Vec<u8>)> = request
            .headers
            .iter()
            .map(|(name, values)| (name.to_string(), join_values(values, b", ")))
            .collect();
        let count = pairs.len();

        let array = alloc_go_strings(&mut ctx.arena, count * 2);
        for (i, (name, value)) in pairs.into_iter().enumerate() {
            let name_ptr = ctx.arena.alloc(name.as_bytes());
            let value_ptr = ctx.arena.alloc(&value);
            // SAFETY: `array` names `count * 2` freshly reserved, writable,
            // properly-aligned `go_string` slots (`alloc_go_strings`); `i * 2`
            // and `i * 2 + 1` are both `< count * 2` since `i < count`, and
            // every index is written exactly once across the loop, so these
            // writes are in-bounds and non-overlapping.
            unsafe {
                array.add(i * 2).write(go_string {
                    len: name.len(),
                    data: name_ptr,
                });
                array.add(i * 2 + 1).write(go_string {
                    len: value.len(),
                    data: value_ptr,
                });
            }
        }

        go_apache_request_headers_return {
            r0: array,
            r1: count,
        }
    })
}

/// frankenphp.go:499-501, minus the `worker` name attribute -- this crate's
/// `RequestContext` has no worker field to read one from (`context.rs`'s own
/// doc comment: worker fields are out of this port's current scope).
fn log_non_http_context() {
    log::log(
        Level::DEBUG,
        || b"apache_request_headers() called in non-HTTP context".to_vec(),
        Vec::new,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;

    use super::*;
    use crate::context::{CompletionSignal, Request, RequestContext};

    // Disjoint from output.rs's 100-119, misc.rs's 1-4 and servervars.rs's
    // 60-76 -- see output.rs's tests module comment for why these must be
    // small and unique across the whole crate's process-global
    // `CONTEXT_SLOTS`.
    const IDX_READ_POST_NO_CONTEXT: usize = 120;
    const IDX_READ_POST_NO_SINK: usize = 121;
    const IDX_READ_POST_FILLS: usize = 122;
    const IDX_COOKIES_NO_CONTEXT: usize = 123;
    const IDX_COOKIES_NO_REQUEST: usize = 124;
    const IDX_HEADERS_NO_CONTEXT: usize = 125;
    const IDX_HEADERS_NO_SINK: usize = 126;
    const IDX_HEADERS_LAYOUT: usize = 127;

    fn fresh_context() -> RequestContext {
        RequestContext::new(String::new(), None, None, CompletionSignal::none())
    }

    fn context_with_request(request: Request) -> RequestContext {
        RequestContext::new(String::new(), None, Some(request), CompletionSignal::none())
    }

    struct FakeSink;
    impl crate::context::ResponseSink for FakeSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn add_header(&mut self, _name: &str, _value: &[u8]) {}
        fn clear_headers(&mut self) {}
        fn write_status(&mut self, _status: u16) {}
        fn flush(&mut self) -> Result<(), crate::context::FlushError> {
            Ok(())
        }
    }

    // -------------------------------------------------------------------
    // go_read_post
    // -------------------------------------------------------------------

    #[test]
    fn go_read_post_with_no_context_returns_zero() {
        let mut buf = [0xffu8; 8];
        assert_eq!(
            go_read_post(IDX_READ_POST_NO_CONTEXT, buf.as_mut_ptr().cast(), buf.len()),
            0
        );
    }

    #[test]
    fn go_read_post_with_no_sink_returns_zero() {
        let idx = IDX_READ_POST_NO_SINK;
        let mut request = Request::new("POST", b"/".to_vec());
        request.body = crate::context::RequestBody::new(&b"hello"[..]);
        CONTEXT_SLOTS.set(idx, context_with_request(request));

        let mut buf = [0xffu8; 8];
        let n = go_read_post(idx, buf.as_mut_ptr().cast(), buf.len());
        CONTEXT_SLOTS.clear(idx);

        assert_eq!(n, 0);
    }

    /// A `Read` impl that yields one byte per call -- exactly the case
    /// `RequestBody::fill`'s own loop exists for (a source returning a short
    /// read is not the same as the body ending).
    struct OneByteAtATime(VecDeque<u8>);
    impl io::Read for OneByteAtATime {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(byte) => {
                    buf[0] = byte;
                    Ok(1)
                }
                None => Ok(0),
            }
        }
    }

    #[test]
    fn go_read_post_fills_the_buffer_completely_from_a_one_byte_source() {
        let idx = IDX_READ_POST_FILLS;
        let mut request = Request::new("POST", b"/".to_vec());
        request.body = crate::context::RequestBody::new(OneByteAtATime(
            b"hello world".iter().copied().collect(),
        ));
        let mut ctx = context_with_request(request);
        ctx.response_sink = Some(Box::new(FakeSink));
        CONTEXT_SLOTS.set(idx, ctx);

        let mut buf = [0u8; 11];
        let n = go_read_post(idx, buf.as_mut_ptr().cast(), buf.len());
        CONTEXT_SLOTS.clear(idx);

        assert_eq!(
            n,
            buf.len(),
            "must loop until the buffer is full, not stop at one byte"
        );
        assert_eq!(&buf, b"hello world");
    }

    // -------------------------------------------------------------------
    // go_read_cookies
    // -------------------------------------------------------------------

    #[test]
    fn go_read_cookies_with_no_context_returns_null() {
        assert!(go_read_cookies(IDX_COOKIES_NO_CONTEXT).is_null());
    }

    #[test]
    fn go_read_cookies_with_no_request_returns_null() {
        let idx = IDX_COOKIES_NO_REQUEST;
        CONTEXT_SLOTS.set(idx, fresh_context());
        let result = go_read_cookies(idx);
        CONTEXT_SLOTS.clear(idx);
        assert!(result.is_null());
    }

    fn read_and_free(ptr: *mut c_char) -> Option<Vec<u8>> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `ptr` came from `malloc_c_string`, a live, NUL-terminated
        // allocation this function takes ownership of and frees below --
        // exactly the contract `go_read_cookies`'s caller
        // (`frankenphp.c:362-365`) relies on.
        let bytes = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes().to_vec();
        // SAFETY: `ptr` was malloc'd by `malloc_c_string` and not yet freed.
        unsafe { frankenrust_sys::free(ptr.cast()) };
        Some(bytes)
    }

    #[test]
    fn go_read_cookies_joins_multiple_cookie_headers_with_semicolon_space() {
        const IDX: usize = 128;
        let request = Request::new("GET", b"/".to_vec())
            .with_header("Cookie", b"a=1".to_vec())
            .with_header("Cookie", b"b=2".to_vec());
        CONTEXT_SLOTS.set(IDX, context_with_request(request));

        let result = read_and_free(go_read_cookies(IDX));
        CONTEXT_SLOTS.clear(IDX);

        assert_eq!(result, Some(b"a=1; b=2".to_vec()));
    }

    #[test]
    fn go_read_cookies_strips_embedded_nul_bytes() {
        const IDX: usize = 129;
        let request = Request::new("GET", b"/".to_vec()).with_header("Cookie", b"a=\x001".to_vec());
        CONTEXT_SLOTS.set(IDX, context_with_request(request));

        let result = read_and_free(go_read_cookies(IDX));
        CONTEXT_SLOTS.clear(IDX);

        assert_eq!(result, Some(b"a=1".to_vec()));
    }

    #[test]
    fn go_read_cookies_with_no_cookie_header_returns_null() {
        const IDX: usize = 130;
        let request = Request::new("GET", b"/".to_vec());
        CONTEXT_SLOTS.set(IDX, context_with_request(request));

        let result = go_read_cookies(IDX);
        CONTEXT_SLOTS.clear(IDX);

        assert!(result.is_null());
    }

    #[test]
    fn go_read_cookies_with_a_single_empty_cookie_value_returns_null() {
        const IDX: usize = 131;
        let request = Request::new("GET", b"/".to_vec()).with_header("Cookie", Vec::new());
        CONTEXT_SLOTS.set(IDX, context_with_request(request));

        let result = go_read_cookies(IDX);
        CONTEXT_SLOTS.clear(IDX);

        assert!(
            result.is_null(),
            "a single Cookie header joining to the empty string must yield NULL"
        );
    }

    // -------------------------------------------------------------------
    // go_apache_request_headers
    // -------------------------------------------------------------------

    #[test]
    fn go_apache_request_headers_with_no_context_returns_null_and_zero() {
        let result = go_apache_request_headers(IDX_HEADERS_NO_CONTEXT);
        assert!(result.r0.is_null());
        assert_eq!(result.r1, 0);
    }

    #[test]
    fn go_apache_request_headers_with_no_sink_returns_null_and_zero() {
        let idx = IDX_HEADERS_NO_SINK;
        let request = Request::new("GET", b"/".to_vec()).with_header("X-Foo", b"bar".to_vec());
        CONTEXT_SLOTS.set(idx, context_with_request(request));

        let result = go_apache_request_headers(idx);
        CONTEXT_SLOTS.clear(idx);

        assert!(result.r0.is_null());
        assert_eq!(result.r1, 0);
    }

    #[test]
    fn go_apache_request_headers_layout_is_pair_count_with_2n_entries_and_joins_repeats() {
        let idx = IDX_HEADERS_LAYOUT;
        let request = Request::new("GET", b"/".to_vec())
            .with_header("X-Multi", b"one".to_vec())
            .with_header("X-Multi", b"two".to_vec())
            .with_header("X-Single", b"solo".to_vec());
        let mut ctx = context_with_request(request);
        ctx.response_sink = Some(Box::new(FakeSink));
        CONTEXT_SLOTS.set(idx, ctx);

        let result = go_apache_request_headers(idx);

        // r1 is the PAIR count (distinct header names), not the element
        // count -- frankenphp.c:770-775 reads `2 * r1` go_strings.
        assert_eq!(result.r1, 2, "two distinct header names were installed");
        assert!(!result.r0.is_null());

        let mut seen = std::collections::HashMap::new();
        for i in 0..result.r1 {
            // SAFETY: `result.r0` names `2 * result.r1` live `go_string`s
            // for the duration of this loop -- the arena backing them is
            // still installed on `idx`'s slot; `CONTEXT_SLOTS.clear(idx)`
            // (which would drop it) runs only after every read below.
            let (key, value) = unsafe {
                let key = &*result.r0.add(i * 2);
                let value = &*result.r0.add(i * 2 + 1);
                (
                    std::slice::from_raw_parts(key.data.cast::<u8>(), key.len).to_vec(),
                    std::slice::from_raw_parts(value.data.cast::<u8>(), value.len).to_vec(),
                )
            };
            seen.insert(key, value);
        }

        CONTEXT_SLOTS.clear(idx);

        assert_eq!(
            seen.get(b"X-Multi".as_slice()),
            Some(&b"one, two".to_vec()),
            "a repeated header must join its values with \", \""
        );
        assert_eq!(seen.get(b"X-Single".as_slice()), Some(&b"solo".to_vec()));
    }
}
