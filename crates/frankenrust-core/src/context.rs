//! The request-context half of `vendor/frankenphp/context.go`: its
//! `frankenPHPContext` (`context.go:16-54`) becomes [`RequestContext`], its
//! `validate()` (`context.go:150-168`) becomes [`validate_request`] (and
//! [`RequestContext::validate`]), and its `closeContext()` (`context.go:135-147`)
//! becomes [`RequestContext::close_context`].
//!
//! This module also owns the per-thread context slot table ([`ContextSlots`],
//! [`CONTEXT_SLOTS`]) -- our analogue of `phpThread.frankenPHPContext()`
//! guarded by `phpThread.contextMu` (`vendor/frankenphp/threadregular.go:129-133`).
//! The thread registry itself (thread state machine, `thread_index` allocation)
//! is a separate issue's job; this module only owns the request-plumbing table
//! keyed by `thread_index` that later callbacks (`$_SERVER` import, request
//! body reads, worker lifecycle) read and write through.
//!
//! # Scope
//!
//! This is deliberately a slice of upstream's `frankenPHPContext`, not the
//! whole thing. Left out, on purpose:
//!
//! - `mercureContext` and `originalRequest` -- Mercure is out of scope for
//!   this port (`docs/PORTING-NOTES.md`), and `originalRequest` only exists to
//!   support it.
//! - The worker fields (`worker`, and the request-recycling they imply) --
//!   worker-mode request handoff is a later issue.
//! - `handlerParameters` / `handlerReturn` -- worker callback plumbing, same
//!   reason.
//! - CGI path splitting (`splitCgiPath`) and document-root resolution: this
//!   module stores `document_root`, `split_path`, `doc_uri`, `path_info`,
//!   `script_name` and `script_filename` exactly as given; computing them is
//!   a separate module's job.
//! - `$_SERVER` / `frankenphp_server_vars` and every `callbacks/` body: no FFI
//!   happens in this file at all.
//! - `RequestBody`'s real (streaming) design: see its doc comment.
//!
//! # The two rules for [`ContextSlots`] callers
//!
//! A [`ContextSlots::with_context`] / [`ContextSlots::with_context_mut`]
//! closure runs with that one slot's lock held. So:
//!
//! 1. **Never call into PHP from inside one.** See that type's doc comment
//!    for why: a Zend bailout `longjmp`s past Rust destructors, so a lock
//!    guard (or anything else) alive on the stack at that moment is never
//!    released.
//! 2. **Never re-enter [`CONTEXT_SLOTS`] at all from inside one** -- not
//!    through `set`, `clear`, `with_context`, `with_context_mut`, nor
//!    anything that calls them (a completion signal fired by
//!    [`RequestContext::close_context`] is the shape to watch for, since
//!    `close_context` is itself reached through `with_context_mut`) -- and
//!    that includes a *different* `thread_index`, not just the same one.
//!    A slot is guarded by a plain `std::sync::Mutex`, which is not
//!    reentrant, so same-index re-entry self-deadlocks; different-index
//!    re-entry does not self-deadlock, but it opens an ABBA hazard the first
//!    version of this module got wrong: thread A holds slot 0's lock and
//!    waits for slot 1, thread B holds slot 1's lock and waits for slot 0,
//!    and there is no lock ordering between independent slots to break the
//!    cycle. [`ContextSlots`] enforces "at most one slot lock per thread" at
//!    runtime, so a violation panics immediately instead of deadlocking
//!    either way -- see its doc comment for why that is the fix, not a
//!    weaker substitute for one.

use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// A multi-valued, case-insensitively-keyed header map.
///
/// Lookup canonicalises the name the way Go's `net/http` does
/// (`textproto.CanonicalMIMEHeaderKey`) on both insert and lookup, since
/// nothing upstream of us has already normalised header casing the way Go's
/// HTTP layer does before a handler ever sees a request.
///
/// Values are raw bytes, not `String`: PHP strings are arbitrary bytes, and a
/// header value must not be assumed to be valid UTF-8.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    entries: Vec<(String, Vec<Vec<u8>>)>,
}

impl Headers {
    pub fn insert(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        let canon = canonical_header_name(name);
        let value = value.into();
        match self.entries.iter_mut().find(|(n, _)| *n == canon) {
            Some((_, values)) => values.push(value),
            None => self.entries.push((canon, vec![value])),
        }
    }

    /// The **first** value for `name` -- Go's `Header.Get`
    /// (`textproto.MIMEHeader.Get`), which returns `v[0]` and never joins
    /// duplicates. This is the accessor [`validate_request`] uses for
    /// `Content-Length`, matching upstream's own call site
    /// (`context.go:157`) rather than a comma-joined view of every
    /// `Content-Length` header the client happened to send.
    ///
    /// `None` if the header was never inserted; `Some(&[])` if it was
    /// inserted with an empty value (Go's `Get` cannot tell the two apart
    /// either).
    pub fn get_first(&self, name: &str) -> Option<&[u8]> {
        let canon = canonical_header_name(name);
        self.entries
            .iter()
            .find(|(n, _)| *n == canon)
            .and_then(|(_, values)| values.first())
            .map(Vec::as_slice)
    }

    /// Every value inserted for `name`, in insertion order. `None` if the
    /// header was never inserted.
    pub fn get_all(&self, name: &str) -> Option<&[Vec<u8>]> {
        let canon = canonical_header_name(name);
        self.entries
            .iter()
            .find(|(n, _)| *n == canon)
            .map(|(_, values)| values.as_slice())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[Vec<u8>])> {
        self.entries
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }
}

/// Port of `net/textproto`'s `validHeaderFieldByte`: the RFC 9110 §5.6.2
/// token bytes, all of which are ASCII.
fn is_header_field_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Port of `textproto.CanonicalMIMEHeaderKey`: upper-cases the first letter
/// and every letter after a `-`, lower-cases the rest.
///
/// Go bails out and returns the name **unchanged** as soon as it meets a byte
/// that is not a valid header-field-name byte, and otherwise only ever flips
/// ASCII letters -- so the canonical form of a well-formed name is pure
/// ASCII. Reproducing that bail-out is what keeps this byte-exact: without
/// it, a name carrying a byte >= 0x80 would be re-encoded by `byte as char`
/// into two UTF-8 bytes and silently corrupted.
fn canonical_header_name(name: &str) -> String {
    if !name.bytes().all(is_header_field_byte) {
        return name.to_string();
    }

    let mut out = String::with_capacity(name.len());
    let mut upper_next = true;
    for byte in name.bytes() {
        let cased = if upper_next {
            byte.to_ascii_uppercase()
        } else {
            byte.to_ascii_lowercase()
        };
        // Every byte here passed `is_header_field_byte`, so it is ASCII and
        // `cased as char` reproduces it exactly (0x00-0x7F round-trips
        // through `char` losslessly).
        out.push(cased as char);
        upper_next = byte == b'-';
    }
    out
}

/// The request body handle #12's `go_read_post` (`callbacks/input.rs`) will
/// eventually read through.
///
/// This is intentionally inert. `RequestBody`'s real design -- almost
/// certainly a streaming handle read incrementally on the PHP thread, the way
/// `go_read_post` reads `fc.request.Body` upstream (`frankenphp.go:683-694`)
/// -- belongs to a later issue, not this one. Building it out here would mean
/// guessing at a shape that issue is free to reject. `Request` derives
/// `Clone`/`Debug` today only because this placeholder is trivially both; a
/// real streaming body is neither (an `io::Read` handle has no meaningful
/// copy), so whichever issue replaces this will have to drop one or both of
/// those derives from `Request` in the same diff. Keeping this field's name
/// and position stable is what keeps that diff small.
#[derive(Debug, Clone, Default)]
pub struct RequestBody;

/// The inbound request. Carries exactly the fields the request-context and
/// CGI layers need (`context.go:23`'s `request *http.Request`, narrowed to
/// what upstream actually reads off it), not a general-purpose HTTP request
/// type -- `frankenrust-core` has no hyper/http dependency
/// (`docs/ARCHITECTURE.md`'s crate-boundary section puts that in
/// `frankenrust-server`), so whatever hands us a `Request` is responsible for
/// filling it in from the real transport.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,

    /// The request-target in **origin form**, undecoded: Go's
    /// `URL.RequestURI()` -- the *method* on `net/url.URL`, which is what
    /// upstream reads (`fc.requestURI = r.URL.RequestURI()`,
    /// `context.go:111`), and deliberately **not** `http.Request.RequestURI`,
    /// the field that holds the request line's target verbatim. "Raw" here
    /// means undecoded, not "exactly as it arrived on the wire".
    ///
    /// Go's method yields the escaped path, then `?` and the raw query
    /// whenever there is a query *or* the client sent a bare `?`
    /// (`URL.ForceQuery`), and `/` for an empty path. It carries **no scheme,
    /// no authority and no fragment** -- even when the client sent an
    /// absolute-form target, `GET http://example.com/a?b=1 HTTP/1.1`, which
    /// RFC 9110 §7.1 allows and proxies do send. A server layer built on the
    /// `http` crate must therefore fill this from
    /// `uri.path_and_query().map(PathAndQuery::as_str)`, falling back to `/`
    /// when there is none, and never from `uri` itself: `Uri`'s own string
    /// form keeps the scheme and authority of an absolute-form target, and
    /// this field reaches PHP as `$_SERVER['REQUEST_URI']`, where that
    /// difference breaks every router matching on a path.
    ///
    /// Percent escapes and that bare trailing `?` survive here unmodified;
    /// neither can be recovered from [`Request::path`] once it has been
    /// decoded, which is why this is a field of its own. `path_and_query()`
    /// preserves both (the `http` crate hands back the raw slice), so the
    /// never-reconstruct invariant and the origin-form rule are satisfiable
    /// at the same time.
    ///
    /// [`RequestContext::new`] copies this straight into `request_uri`: see
    /// that field's doc comment for why it must never be rebuilt from `path`
    /// and `query` instead.
    pub raw_target: Vec<u8>,

    /// The **decoded** request path (Go's `URL.Path`). What CGI path
    /// splitting (a separate module) works on.
    ///
    /// Bytes, not `String`: percent-decoding a request target produces
    /// arbitrary octets (`%FF` decodes to the byte `0xFF`), and PHP strings
    /// are byte strings -- `PATH_INFO`, `SCRIPT_NAME` and `SCRIPT_FILENAME`
    /// all derive from this field and all reach `$_SERVER` as raw bytes.
    /// Forcing `String` here would mean rejecting or lossily replacing any
    /// non-UTF-8 byte on the way in, which is not upstream's behaviour. This
    /// is the byte-vs-`String` choice a later issue owns revisiting; until
    /// then this is the type.
    pub path: Vec<u8>,

    /// The raw, undecoded query string (Go's `URL.RawQuery`), without a
    /// leading `?`. Reaches PHP as `QUERY_STRING` unmodified.
    pub query: Vec<u8>,

    pub headers: Headers,

    /// The body length the transport parsed -- Go's
    /// `http.Request.ContentLength`, not the `Content-Length` header. `-1`
    /// means *unknown*, which is what a chunked body reports and what no
    /// header value can express; `0` means "no body" (or, as in Go, unknown
    /// for the rare request that has a body and no length).
    ///
    /// This is a field of its own, rather than something recovered from
    /// [`Request::headers`], because upstream copies it straight into the
    /// SAPI: `info.content_length = C.zend_long(request.ContentLength)`
    /// (`cgi.go:304`). The header keeps its own two jobs, and neither is this
    /// one: `$_SERVER['CONTENT_LENGTH']` is `request.Header.Get("Content-Length")`
    /// (`cgi.go:92`), and so is what [`validate_request`] parses
    /// (`context.go:157`). Do not unify the three -- upstream reads each
    /// deliberately, and for a chunked request they legitimately disagree
    /// (`-1` here, no header at all there).
    pub content_length: i64,

    pub host: String,
    pub remote_addr: String,
    pub proto_major: u16,
    pub proto_minor: u16,

    /// The Rust encoding `docs/PORTING-NOTES.md`'s construct-mapping table
    /// prescribes project-wide for `context.Context` cancellation:
    /// `Arc<AtomicBool>` on the pthread side, paired with a `Notify` the
    /// async side owns (out of scope here). Whatever bridges the transport
    /// to this request flips this to `true` on client disconnect;
    /// [`RequestContext::client_has_closed`] only ever reads it, and a later
    /// issue's `go_is_context_done` callback will too.
    pub cancelled: Arc<AtomicBool>,

    pub body: RequestBody,
}

impl Request {
    pub fn new(method: impl Into<String>, path: impl Into<Vec<u8>>) -> Self {
        Self {
            method: method.into(),
            raw_target: Vec::new(),
            path: path.into(),
            query: Vec::new(),
            headers: Headers::default(),
            // Go's zero value, and what net/http reports for a request with
            // no body; a transport that knows better (chunked -> -1) sets it.
            content_length: 0,
            host: String::new(),
            remote_addr: String::new(),
            proto_major: 1,
            proto_minor: 1,
            cancelled: Arc::new(AtomicBool::new(false)),
            body: RequestBody,
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<Vec<u8>>) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// See [`Request::raw_target`]. Whatever fills a `Request` in from the
    /// wire owns setting this to the literal request-target bytes.
    pub fn with_raw_target(mut self, raw_target: impl Into<Vec<u8>>) -> Self {
        self.raw_target = raw_target.into();
        self
    }

    /// See [`Request::content_length`]: the *parsed* length, `-1` for a body
    /// of unknown length. Setting it does not add a `Content-Length` header,
    /// and adding that header does not set this.
    pub fn with_content_length(mut self, content_length: i64) -> Self {
        self.content_length = content_length;
        self
    }

    pub fn with_query(mut self, query: impl Into<Vec<u8>>) -> Self {
        self.query = query.into();
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_remote_addr(mut self, remote_addr: impl Into<String>) -> Self {
        self.remote_addr = remote_addr.into();
        self
    }

    pub fn with_proto(mut self, major: u16, minor: u16) -> Self {
        self.proto_major = major;
        self.proto_minor = minor;
        self
    }
}

/// The decision half of `fc.validate()` / `fc.reject()` (`context.go:150-207`).
/// Upstream's `reject()` writes this straight to an `http.ResponseWriter`;
/// this module does not own a response writer, so `RejectedRequest` only
/// carries the verdict -- status and message -- for whoever renders one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRequest {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for RejectedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RejectedRequest {}

/// Port of Go's `%q` (`strconv.Quote`) over a **byte** string, which is what
/// `fmt.Errorf("%w: %q", ...)` (`context.go:160`) applies to the header value:
/// a Go `string` is arbitrary bytes, and `Quote` renders the ones that are not
/// valid UTF-8 as `\xNN` rather than losing them. `String::from_utf8_lossy`
/// would replace each with U+FFFD, so a `Content-Length: \xff` would be
/// reported as `"\u{fffd}"` and the offending byte would be unrecoverable
/// from the message.
///
/// Follows `strconv.Quote` (`strconv/quote.go`, `appendQuotedWith` /
/// `appendEscapedRune`): an invalid UTF-8 sequence emits its *first* byte as
/// `\xNN` and advances one byte (Go's `utf8.DecodeRuneInString` returns width
/// 1 for any invalid encoding, so a truncated 3-byte sequence yields two
/// escapes, not one replacement character); `"` and `\` are always
/// backslashed; printable ASCII is literal; `\a \b \f \n \r \t \v` win over
/// the hex form; other bytes below 0x20 and 0x7f are `\xNN`; a non-printable
/// rune is `\uXXXX` below 0x10000 and `\UXXXXXXXX` above -- lower-case hex
/// throughout.
///
/// # Where this is not byte-exact, and why that is the floor
///
/// Go's `strconv.IsPrint` is "Unicode categories L, M, N, P, S, plus ASCII
/// space"; the oracle [`quote_rune`] uses for it is `str::escape_debug` in
/// non-leading position, which is the same predicate (see there). Compared
/// scalar by scalar against `strconv.IsPrint` on go1.26, the two agree on
/// 1,101,446 of the 1,112,064 Unicode scalar values -- plus `"`, `'` and `\`,
/// which the oracle escapes and Go does not, but which are ASCII and so are
/// decided by [`quote_rune`] before the oracle is consulted.
///
/// Every one of the 10,615 disagreements is category Cn in go1.26's tables:
/// the two toolchains generate from different Unicode versions, so go1.26
/// (15.0.0) escapes a scalar assigned after it (CJK Ext I and J, for instance)
/// as unassigned-hence-unprintable, while a newer rustc prints it. That is not
/// fixable by us in any stable sense -- it moves whenever either toolchain
/// updates. End to end that leaves `go_quote` byte-identical to
/// `strconv.Quote` over a 2,424,384-string corpus (every single byte, every
/// scalar alone and embedded, 200k random byte soups) on **every input that
/// does not contain a post-15.0.0 scalar**, and the residue is confined to a
/// diagnostic string for a request that is already a 400. It cannot lose
/// information the way the lossy conversion this replaced did, either: the
/// bytes are still recoverable from the message.
fn go_quote(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');

    let mut rest = bytes;
    while !rest.is_empty() {
        let valid_up_to = match std::str::from_utf8(rest) {
            Ok(valid) => {
                for c in valid.chars() {
                    quote_rune(&mut out, c);
                }
                break;
            }
            Err(e) => e.valid_up_to(),
        };

        let (valid, invalid) = rest.split_at(valid_up_to);
        for c in std::str::from_utf8(valid)
            .expect("bytes below valid_up_to are valid UTF-8 by definition")
            .chars()
        {
            quote_rune(&mut out, c);
        }
        out.push_str(&format!("\\x{:02x}", invalid[0]));
        rest = &invalid[1..];
    }

    out.push('"');
    out
}

/// One rune of [`go_quote`]: Go's `appendEscapedRune` with `quote == '"'` and
/// neither `ASCIIonly` nor `graphicOnly` set, which is what `%q` on a string
/// uses.
fn quote_rune(out: &mut String, c: char) {
    if c == '"' || c == '\\' {
        out.push('\\');
        out.push(c);
        return;
    }

    let printable = if c.is_ascii() {
        // Go: `r < utf8.RuneSelf && IsPrint(r)`, and IsPrint over ASCII is
        // exactly 0x20..=0x7e.
        (' '..='~').contains(&c)
    } else {
        // Go's `strconv.IsPrint` is categories L, M, N, P, S plus ASCII space.
        // Rust's own printability predicate -- the one behind escape_debug --
        // is the complement of Cc/Cf/Cs/Co/Cn/Zl/Zp/Zs-minus-ASCII-space,
        // which is the same set; `char::escape_debug` diverges from Go only
        // because it *additionally* escapes grapheme-extended scalars, and
        // category M is grapheme-extended in bulk. So a combining acute came
        // out as a backslash-u-0301 escape where Go emits the mark itself.
        //
        // `str::escape_debug` documents that "only extended grapheme
        // codepoints that begin the string will be escaped", so putting `c` in
        // non-leading position turns that extra rule off and leaves exactly
        // Go's predicate. `'a'` is the lead-in: printable, non-escaped, ASCII,
        // one byte. See go_quote's doc comment for the measured agreement.
        let mut buf = [0u8; 5];
        buf[0] = b'a';
        c.encode_utf8(&mut buf[1..]);
        let probe = std::str::from_utf8(&buf[..1 + c.len_utf8()])
            .expect("'a' followed by one encoded char is valid UTF-8");

        let mut escaped = probe.escape_debug();
        escaped.next() == Some('a') && escaped.next() == Some(c) && escaped.next().is_none()
    };
    if printable {
        out.push(c);
        return;
    }

    match c {
        '\u{7}' => out.push_str("\\a"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{b}' => out.push_str("\\v"),
        _ => {
            let code = c as u32;
            if code < 0x20 || code == 0x7f {
                out.push_str(&format!("\\x{code:02x}"));
            } else if code < 0x1_0000 {
                out.push_str(&format!("\\u{code:04x}"));
            } else {
                out.push_str(&format!("\\U{code:08x}"));
            }
        }
    }
}

/// Port of `fc.validate()` (`context.go:150-168`) -- the decision only, not
/// `reject()`'s response-writing side effect.
pub fn validate_request(request: &Request) -> Result<(), RejectedRequest> {
    if request.path.contains(&0) {
        return Err(RejectedRequest {
            status: 400,
            message: "invalid request path".to_string(),
        });
    }

    // Header.Get (context.go:157) reads the *first* value, not a join of
    // every Content-Length the client sent: two "Content-Length: 5" headers
    // must validate exactly as one does upstream, rather than be rejected as
    // the non-numeric "5, 5".
    if let Some(content_length) = request.headers.get_first("Content-Length") {
        if !content_length.is_empty() {
            let parsed = std::str::from_utf8(content_length)
                .ok()
                .and_then(|s| s.parse::<i64>().ok());
            match parsed {
                Some(n) if n >= 0 => {}
                _ => {
                    return Err(RejectedRequest {
                        status: 400,
                        // `fmt.Errorf("%w: %q", ...)` over the raw header
                        // bytes (context.go:160) -- see go_quote for why not
                        // `{:?}` over a lossy String.
                        message: format!(
                            "invalid Content-Length header: {}",
                            go_quote(content_length)
                        ),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Port of `fc.done` (`context.go:52`), the `chan any` `closeContext` closes
/// (`context.go:145`) to release whatever is awaiting the response. Exactly
/// one waiter per request, released exactly once.
///
/// # Why a boxed closure and not a channel end
///
/// The two ends of this signal sit on opposite sides of the async<->pthread
/// boundary and are built from different primitives on purpose:
///
/// - The *firing* end is a PHP pthread, running inside
///   [`RequestContext::close_context`]. `frankenrust-core` is the PHP side of
///   this port (`docs/ARCHITECTURE.md`) and must not reach for
///   `tokio::sync::oneshot` itself (`docs/PORTING-NOTES.md`'s construct
///   mapping table is explicit that PHP-side channels are not tokio's), so
///   this crate cannot name the async runtime's type here at all.
/// - The *waiting* end is a tokio task in `frankenrust-server`, and that side
///   must `await`, not block a runtime thread for the length of a script run
///   -- a `std::sync::mpsc::Receiver` cannot serve it, since it is not a
///   `Future`.
///
/// So the signal is supplied by whoever builds the request: `frankenrust-server`
/// wraps a `tokio::sync::oneshot::Sender` (whose `send` consumes the sender,
/// never blocks, and is callable from any thread) in the closure passed to
/// [`CompletionSignal::new`]; this module never sees that this crate is
/// tokio-free, and tests use [`CompletionSignal::none`].
///
/// # What the closure may do
///
/// It runs on the PHP thread, inside [`RequestContext::close_context`], with
/// the request's context-slot lock held by whoever called it (see
/// [`ContextSlots`]) -- so it must not block, must not call into PHP, must
/// not panic, and must not touch [`CONTEXT_SLOTS`] for its own
/// `thread_index` (that is [`ContextSlots`]'s rule 2, and this closure is the
/// likeliest place to break it: clearing the slot from here would deadlock
/// against the very lock that reached `close_context`). Waking a oneshot
/// receiver satisfies all four.
#[derive(Default)]
pub struct CompletionSignal(Option<Box<dyn FnOnce() + Send>>);

impl CompletionSignal {
    /// The signal `frankenrust-server` builds around its oneshot sender.
    pub fn new(signal: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(signal)))
    }

    /// No waiter at all -- for contexts nobody is awaiting (tests, and any
    /// probe request a later issue's thread lifecycle needs).
    pub fn none() -> Self {
        Self(None)
    }

    /// `close(fc.done)`: fires at most once, however often it is called.
    /// [`RequestContext::close_context`] is already idempotent through
    /// `is_done`; this is the belt to that pair of braces, and it is what
    /// lets the closure be `FnOnce` (and so hold a consuming
    /// `oneshot::Sender`).
    fn fire(&mut self) {
        if let Some(signal) = self.0.take() {
            signal();
        }
    }
}

impl std::fmt::Debug for CompletionSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CompletionSignal")
            .field(&if self.0.is_some() { "pending" } else { "fired" })
            .finish()
    }
}

/// Allocation-stable arena for the strings `SG(request_info)` borrows for the
/// request's whole lifetime. `frankenphp_free_request_context`
/// (`frankenphp.c:361-373`) NULLs the five fields it pinned rather than
/// freeing them -- ownership of that memory stays on our side of the FFI
/// boundary for as long as the request lives, which is exactly what this
/// arena is for.
///
/// Buffers are one `Box<[u8]>` per `alloc` call, never a single growing
/// `Vec<u8>` handing out interior slices: address stability is this type's
/// defining property, and a bump arena's first reallocation would dangle
/// every pointer C already holds. A `Vec` of `Box`es moves the *handles* when
/// it grows and never the bytes behind them. `Box` rather than a shared
/// handle (`Arc`, `Rc`) for a second reason, spelled out in `alloc`'s SAFETY
/// comment: only a uniquely-owned buffer can hand out a pointer that C is
/// allowed to write through, and `sapi_request_info`'s fields are `char *`.
#[derive(Default)]
pub struct RequestArena {
    buffers: Vec<Box<[u8]>>,
}

impl RequestArena {
    /// Copies `bytes` into a new heap allocation owned by this arena, appends
    /// a trailing NUL (PHP's SAPI reads several of these fields as C
    /// strings), and returns a pointer to it.
    ///
    /// Interior NUL bytes in `bytes` are preserved as-is rather than
    /// rejected: a C reader using `strlen` simply stops at the first one,
    /// same as it would for any other embedded-NUL C string this port hands
    /// across the FFI boundary.
    ///
    /// SAFETY (for callers that go on to dereference the returned pointer),
    /// in two halves -- *lifetime* and *provenance*:
    ///
    /// **Lifetime.** The pointer is valid for exactly as long as this
    /// `RequestArena` (and so the `RequestContext` that owns it) is alive.
    /// Each call pushes a new, independent `Box<[u8]>` into `self.buffers`;
    /// that `Box` owns one heap allocation, separate from the `Vec`'s own
    /// backing storage. Growing `self.buffers` (a `Vec` of 16-byte fat
    /// pointers -- address plus length) reallocates and moves *those* handles
    /// around, but never the allocation any one of them points to, and that
    /// allocation is not freed while its `Box` stays in `self.buffers`. So a
    /// pointer returned by an earlier call stays valid across later ones, and
    /// for the rest of the request after that. It stops being valid the
    /// moment this arena drops -- see [`RequestContext::close_context`] for
    /// why that is not the same moment as the request being marked done.
    ///
    /// **Provenance.** The pointer is derived from `&mut **last_mut()`, a
    /// unique borrow of the buffer's own allocation, so it is valid for
    /// **writes** as well as reads. That is not a detail: every field this
    /// feeds is declared `char *`, not `const char *`, in `sapi_request_info`
    /// (`frankenphp.c:367-372` NULLs `request_method`, `query_string`,
    /// `content_type`, `path_translated` and `request_uri`), so the C
    /// signature we hand it to promises a writable buffer. Deriving the
    /// pointer from a shared reference instead -- `Arc::as_ptr`, or any
    /// `&[u8] as *mut` -- keeps the address and silently loses the
    /// permission: the cast compiles, and the first write through it is UB
    /// (Miri: "only grants SharedReadOnly permission for this location").
    /// The `&mut` is also taken *after* the push, from the buffer in its
    /// final home, because moving the `Box` into the `Vec` afterwards would
    /// invalidate a pointer derived before the move. No second pointer into
    /// the allocation is ever handed out, so C's writes alias nothing of
    /// ours.
    pub fn alloc(&mut self, bytes: &[u8]) -> *mut c_char {
        let mut buf = Vec::with_capacity(bytes.len() + 1);
        buf.extend_from_slice(bytes);
        buf.push(0);
        self.buffers.push(buf.into_boxed_slice());
        self.buffers
            .last_mut()
            .expect("just pushed")
            .as_mut_ptr()
            .cast::<c_char>()
    }
}

/// Port of `frankenPHPContext` (`context.go:16-54`). See this module's doc
/// comment for what is intentionally not ported.
pub struct RequestContext {
    pub document_root: String,
    pub split_path: Vec<String>,
    pub request: Option<Request>,

    /// Derived from [`Request::path`], and destined for `$_SERVER` /
    /// `SG(request_info)` as raw bytes for the same reason `path` is. Stored
    /// exactly as given -- computing these from `request` is CGI path
    /// splitting's job, not this constructor's.
    pub doc_uri: Vec<u8>,
    pub path_info: Vec<u8>,
    pub script_name: Vec<u8>,
    pub script_filename: Vec<u8>,

    /// Upstream: `fc.requestURI = r.URL.RequestURI()` (`context.go:111`).
    /// [`RequestContext::new`] sets this to [`Request::raw_target`], copied
    /// verbatim -- **never** rebuilt from `path` + `query`. A decoded path
    /// cannot represent a percent-escape (it has already been decoded) or a
    /// bare trailing `?` with an empty query (`URL.ForceQuery` -- a decoded
    /// path plus an empty query string is indistinguishable from "no query
    /// at all"), and `URL.RequestURI()` preserves both.
    pub request_uri: Vec<u8>,

    /// Whether the request is already closed by us (`context.go:37`).
    pub is_done: bool,

    /// The client's connection state as of the moment `is_done` was set
    /// (`context.go:38-45`). Captured once, inside `close_context`, before
    /// the completion signal fires: firing is what lets the awaiting HTTP
    /// handler return, which is itself what cancels the request on the
    /// transport side -- so reading connection state *after* firing would
    /// read "closed" for virtually any write following a normal
    /// `fastcgi_finish_request()`, not just a real client abort.
    pub client_had_closed: bool,

    completion_signal: CompletionSignal,

    pub arena: RequestArena,
}

impl RequestContext {
    /// `document_root` and `split_path` are stored exactly as given --
    /// resolving an empty document root, and validating/normalising
    /// `split_path`, both belong to the module that owns CGI path splitting.
    /// `doc_uri`, `path_info`, `script_name` and `script_filename` start
    /// empty for the same reason: computing them from `request` is that
    /// module's job too, not this constructor's.
    pub fn new(
        document_root: String,
        split_path: Vec<String>,
        request: Option<Request>,
        completion_signal: CompletionSignal,
    ) -> Self {
        // See `request_uri`'s doc comment: copied verbatim, never rebuilt.
        let request_uri = request
            .as_ref()
            .map(|r| r.raw_target.clone())
            .unwrap_or_default();

        Self {
            document_root,
            split_path,
            request,
            doc_uri: Vec::new(),
            path_info: Vec::new(),
            script_name: Vec::new(),
            script_filename: Vec::new(),
            request_uri,
            is_done: false,
            client_had_closed: false,
            completion_signal,
            arena: RequestArena::default(),
        }
    }

    /// Port of `fc.validate()` (`context.go:150-168`), delegating to
    /// [`validate_request`]. A context with no request has nothing to
    /// reject.
    pub fn validate(&self) -> Result<(), RejectedRequest> {
        match &self.request {
            Some(request) => validate_request(request),
            None => Ok(()),
        }
    }

    /// Port of `clientHasClosed` (`context.go:171-182`).
    pub fn client_has_closed(&self) -> bool {
        match &self.request {
            Some(request) => request.cancelled.load(Ordering::SeqCst),
            None => false,
        }
    }

    /// Port of `closeContext` (`context.go:135-147`): idempotent, snapshots
    /// the client's connection state (see [`RequestContext::client_had_closed`]),
    /// fires the completion signal, then marks `is_done`.
    ///
    /// Does **not** touch `arena`. The arena's reclaim point is its owning
    /// `RequestContext` being dropped -- when the per-thread slot that holds
    /// it (see [`ContextSlots`]) is cleared or replaced -- not response
    /// completion. `go_frankenphp_finish_php_request`
    /// (`vendor/frankenphp/threadworker.go:328-336`) calls `closeContext()`
    /// but deliberately does not clear the slot, because a script that calls
    /// `fastcgi_finish_request()` keeps running -- and keeps writing through
    /// `SG(request_info)` -- afterwards.
    pub fn close_context(&mut self) {
        if self.is_done {
            return;
        }
        self.client_had_closed = self.client_has_closed();
        self.completion_signal.fire();
        self.is_done = true;
    }
}

/// Per-thread request-context slots, keyed by `thread_index` -- our analogue
/// of `phpThread.frankenPHPContext()` guarded by `phpThread.contextMu`
/// (`threadregular.go:129-133`).
///
/// `slots` is an `RwLock` guarding only *growth* (a rare event: the table
/// grows once per newly-seen `thread_index`); each thread's own `Mutex`
/// guards its slot for the hot path (set/get/clear on every request), so
/// unrelated PHP threads never contend with each other the way one global
/// lock over the whole table would make them. Each slot is behind its own
/// `Arc` so that every accessor can *clone the slot out and drop the
/// table-level guard* before it does anything else -- see
/// [`ContextSlots::slot`].
///
/// # Rule 1: never call into PHP from a closure
///
/// **Never call into PHP from inside a [`ContextSlots::with_context`] /
/// [`ContextSlots::with_context_mut`] closure.** Copy out what you need,
/// return, and call C (or anything that can reach PHP) afterwards.
///
/// Any Zend routine can end in `zend_bailout()` -- memory-limit exhaustion
/// being the ordinary case -- which is a `longjmp` to a `zend_catch` sitting
/// *above* our frames on every path into a callback that reaches this table.
/// A `longjmp` runs no Rust destructors: a guard alive across such a call is
/// leaked, and the slot it guards is locked *forever*. Worse, the very next
/// thing C does on that path is call back in to clear this same slot, which
/// would then deadlock the PHP thread inside its own crash-recovery path --
/// the request would never be answered, and (because a leaked read guard
/// also pins the table's reader count) no *other* thread's first-ever
/// `thread_index` could grow the table again either.
///
/// Upstream has the same shape, for the same reason: `contextMu` guards only
/// the store (`threadregular.go:119-122`, `:131-134`), and the hot-path
/// reader `frankenPHPContext()` (`threadregular.go:77-79`) takes no lock at
/// all. `docs/PORTING-NOTES.md` states the rule in one line: avoid holding a
/// lock across an FFI call into PHP.
///
/// Note that releasing the guard before calling PHP is necessary but not
/// sufficient on its own: Rust has no defined behaviour for a `longjmp`
/// crossing *any* of its frames, not merely ones holding a destructor. The
/// shape that is actually sound is a C entry point that calls into Rust to
/// compute a value, returns, and only *then* calls the PHP routine that can
/// bail out -- never Rust calling a bail-out-capable PHP function directly
/// from a frame still on the stack. That is a constraint on the callbacks
/// that use this table, not on the table itself; it is recorded here because
/// this is where a reviewer will look for it.
///
/// # Rule 2: never hold more than one slot lock at a time
///
/// Rule 1 grants a reader licence to do any PHP-free work inside the closure,
/// and touching this table again is PHP-free -- so it is tempting to think
/// re-entering for a *different* `thread_index` is safe. It is not, and an
/// earlier version of this module said it was safe and shipped tests
/// asserting exactly that; a reviewer reproduced the counterexample
/// deterministically with two threads and a barrier: thread A calls
/// `with_context(0, ..)` and, inside that closure, `with_context(1, ..)`;
/// thread B does the same in the opposite order (`with_context(1, ..)` then
/// `with_context(0, ..)`). Each slot is an independent
/// `std::sync::Mutex` with no ordering relationship between them, so A can
/// hold 0 and wait for 1 at the same instant B holds 1 and waits for 0 --
/// classic ABBA, and there is no way to fix it by choosing a "safe" order at
/// each call site, because two *different* call sites choosing two
/// *different* orders is exactly the bug.
///
/// The only deadlock-proof rule is the stricter one: a thread may hold at
/// most one slot lock at any instant, full stop, whether the second attempt
/// targets the same index (self-deadlock on a non-reentrant `Mutex`) or a
/// different one (the ABBA case above). [`ContextSlots`] enforces this with
/// [`SingleSlotGuard`], a thread-local flag `set`, `clear`, `with_context`
/// and `with_context_mut` each raise on entry and lower on exit: a second
/// acquisition attempt on the same thread panics *before* it ever touches a
/// `Mutex`, so no lock is ever taken while another is held and no hold-and-
/// wait cycle can form -- this is enforced by code, not merely documented,
/// because documentation alone is exactly what let the ABBA case ship the
/// first time.
///
/// This is a deliberate departure from [`recover_lock`]/[`recover_read`]'s
/// philosophy of never panicking in this table (a poisoned `Option` is safe
/// to keep serving, so recovering avoids aborting the process over an
/// unrelated bug). A same-thread re-entry is not that kind of bug: it is a
/// violation of this table's own contract, indistinguishable from a live
/// deadlock to anything downstream, and a hung PHP worker thread is worse
/// than a loud panic -- it never surfaces as a poisoned lock, it just quietly
/// removes one thread's worth of capacity forever. Failing fast here is
/// strictly more visible than the failure mode it replaces.
///
/// Growing the table from inside a closure remains fine on its own terms --
/// [`ContextSlots::slot`] clones the slot's `Arc` out and drops the
/// table-level `RwLock` guard before the closure ever runs, so a nested `set`
/// on a not-yet-seen index never blocks on the write lock while the caller
/// holds a read lock -- but it is reached through `set`, which
/// [`SingleSlotGuard`] gates like every other entry point, so nesting it
/// inside another slot's closure still panics. Only *non-nested* growth is
/// exercised.
pub struct ContextSlots {
    slots: RwLock<Vec<Arc<Mutex<Option<RequestContext>>>>>,
}

/// Read through these two helpers rather than `.unwrap()`, deliberately.
///
/// Every caller of this table is reached from an `extern "C"` callback with
/// no unwind guard, so a panicking `.unwrap()` on a poisoned lock would not
/// fail just that one request -- it would unwind across the FFI boundary and
/// abort the whole process. Poisoning would then be permanent: one panic
/// anywhere under a slot guard turns every later access to that thread's
/// slot into a hard kill of the server.
///
/// Recovering is sound here because the guarded value carries no multi-step
/// invariant a panic could leave half-established: it is an
/// `Option<RequestContext>`, and Rust guarantees the `Option` itself is
/// intact whatever the panic interrupted. The worst a recovered lock can
/// expose is a context whose arena grew by fewer buffers than intended,
/// which is exactly the state an abandoned request is entitled to.
fn recover_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recover_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

std::thread_local! {
    /// Whether *this* OS thread currently holds a [`ContextSlots`] slot lock.
    /// See [`SingleSlotGuard`] and [`ContextSlots`]'s "Rule 2" doc section:
    /// this is what turns a same-thread second acquisition -- same index or
    /// not -- into an immediate panic instead of a self-deadlock or an ABBA
    /// deadlock against another thread.
    static HOLDING_SLOT_LOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII enforcement of "at most one [`ContextSlots`] slot lock per thread".
/// Every public entry point (`set`, `clear`, `with_context`,
/// `with_context_mut`) acquires one of these *before* it locks its slot's
/// `Mutex`, and holds it for exactly as long as that `Mutex` guard is held.
///
/// # Why this is a panic, not a `Result`
///
/// A second acquisition on the same thread is never a legitimate race --
/// it is a caller violating this table's own contract from inside its own
/// call stack, which no other thread can trigger and no timing can avoid.
/// Returning a `Result` would just move the "what do I do about a bug"
/// question to a caller with no better answer than panicking; failing here,
/// loudly, at the exact call site that broke the rule, is more debuggable
/// than either a hang (the bug this replaces) or a silently-skipped access
/// (the alternative of using `try_lock` and pretending the slot was empty).
///
/// SAFETY-adjacent note: this is deliberately *not* a `Mutex`/`RwLock`
/// re-entrancy check delegated to the OS lock itself (e.g. via `try_lock`)
/// -- the panic must fire before any second `Mutex::lock` call is attempted,
/// so that the offending thread never blocks even transiently. A
/// thread-local flag checked-then-set ahead of the real lock is what
/// guarantees that ordering.
struct SingleSlotGuard;

impl SingleSlotGuard {
    fn acquire() -> Self {
        HOLDING_SLOT_LOCK.with(|held| {
            assert!(
                !held.get(),
                "ContextSlots: this thread already holds a slot lock; nesting a second \
                 set/clear/with_context/with_context_mut call inside one -- same \
                 thread_index or not -- is forbidden (see ContextSlots' doc comment: \
                 different-index nesting is an ABBA deadlock waiting for a second thread \
                 nesting in the opposite order)"
            );
            held.set(true);
        });
        Self
    }
}

impl Drop for SingleSlotGuard {
    fn drop(&mut self) {
        // Runs even when unwinding past this guard (a closure that panics,
        // as in the poisoned-slot test below), so a caught panic leaves this
        // thread able to acquire again rather than permanently locked out.
        HOLDING_SLOT_LOCK.with(|held| held.set(false));
    }
}

impl ContextSlots {
    pub const fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
        }
    }

    /// `thread_index`'s slot, growing the table if this index has not been
    /// seen before.
    ///
    /// Every accessor goes through here, and every one of them holds **no
    /// table-level guard** by the time it locks the slot: this returns an
    /// owned `Arc`, and both the read guard on the fast path and the write
    /// guard on the growth path die inside this function. That keeps the
    /// table-level `RwLock` out of the deadlock analysis entirely -- the only
    /// lock a caller can be holding once its closure runs is the one slot's
    /// `Mutex`, and [`SingleSlotGuard`] is what keeps that count at "at most
    /// one" (see this type's doc comment for why a second slot, not just the
    /// same one, must be included in that limit).
    fn slot(&self, thread_index: usize) -> Arc<Mutex<Option<RequestContext>>> {
        // Bound as its own statement so the read guard is released here, not
        // at the end of an enclosing `if let`: the write lock below would
        // otherwise be taken while this thread still held a read lock.
        let existing = recover_read(&self.slots).get(thread_index).map(Arc::clone);
        if let Some(slot) = existing {
            return slot;
        }

        let mut slots = recover_write(&self.slots);
        while slots.len() <= thread_index {
            slots.push(Arc::new(Mutex::new(None)));
        }
        Arc::clone(&slots[thread_index])
    }

    /// Installs `ctx` as `thread_index`'s context, dropping (and so
    /// releasing the arena of) whatever context previously occupied the
    /// slot, if any.
    pub fn set(&self, thread_index: usize, ctx: RequestContext) {
        let _guard = SingleSlotGuard::acquire();
        let slot = self.slot(thread_index);
        *recover_lock(&slot) = Some(ctx);
    }

    /// Drops `thread_index`'s context, if any -- releasing its arena.
    pub fn clear(&self, thread_index: usize) {
        let _guard = SingleSlotGuard::acquire();
        let slot = self.slot(thread_index);
        *recover_lock(&slot) = None;
    }

    /// Runs `f` with `thread_index`'s context, holding that slot's lock for
    /// the duration. `f` must not call into PHP, and must not re-enter this
    /// table at all, for `thread_index` or any other -- see this type's two
    /// rules for callers. A violation panics via [`SingleSlotGuard`] rather
    /// than deadlocking.
    pub fn with_context<R>(
        &self,
        thread_index: usize,
        f: impl FnOnce(Option<&RequestContext>) -> R,
    ) -> R {
        let _guard = SingleSlotGuard::acquire();
        let slot = self.slot(thread_index);
        let guard = recover_lock(&slot);
        f(guard.as_ref())
    }

    /// Mutable counterpart of [`ContextSlots::with_context`], needed by
    /// anything that pushes into the context's arena. The same two rules
    /// apply.
    pub fn with_context_mut<R>(
        &self,
        thread_index: usize,
        f: impl FnOnce(Option<&mut RequestContext>) -> R,
    ) -> R {
        let _guard = SingleSlotGuard::acquire();
        let slot = self.slot(thread_index);
        let mut guard = recover_lock(&slot);
        f(guard.as_mut())
    }
}

impl Default for ContextSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// The one instance every callback that needs request context reads and
/// writes through.
pub static CONTEXT_SLOTS: ContextSlots = ContextSlots::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(request: Option<Request>) -> RequestContext {
        RequestContext::new(String::new(), Vec::new(), request, CompletionSignal::none())
    }

    /// Runs `body` on a scratch thread and fails the test if it has not
    /// finished within ten seconds.
    ///
    /// The callers below are deadlock regression tests, and a deadlock is not
    /// a test failure by default -- it is a `cargo test` that never returns,
    /// which in the gate reads as a hung machine rather than as a red test.
    /// This turns one into the other.
    fn run_or_fail_on_deadlock(what: &str, body: impl FnOnce() + Send + 'static) {
        use std::sync::mpsc::RecvTimeoutError;

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            body();
            let _ = done_tx.send(());
        });

        match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            // Disconnected without a send means `body` panicked; join re-raises it.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                worker.join().expect("watched thread must not panic");
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("{what}: still running after 10s -- deadlocked")
            }
        }
    }

    #[test]
    fn headers_lookup_is_canonicalisation_insensitive() {
        let mut headers = Headers::default();
        headers.insert("accept-ENCODING", b"gzip".to_vec());
        assert_eq!(
            headers.get_first("Accept-Encoding"),
            Some(b"gzip".as_slice()),
            "lookup must canonicalise regardless of how the name was inserted"
        );
    }

    #[test]
    fn headers_get_first_is_the_first_value_not_a_join() {
        let mut headers = Headers::default();
        headers.insert("Content-Type", b"text/plain".to_vec());
        headers.insert("Content-Type", b"application/json".to_vec());

        assert_eq!(
            headers.get_first("Content-Type"),
            Some(b"text/plain".as_slice())
        );
        assert_eq!(
            headers.get_all("Content-Type"),
            Some([b"text/plain".to_vec(), b"application/json".to_vec()].as_slice())
        );
        assert_eq!(headers.get_first("Missing"), None);
        assert_eq!(headers.get_all("Missing"), None);
    }

    #[test]
    fn headers_present_but_empty_is_some_empty() {
        let mut headers = Headers::default();
        headers.insert("Content-Length", Vec::new());
        assert_eq!(headers.get_first("Content-Length"), Some([].as_slice()));
    }

    #[test]
    fn canonical_header_name_leaves_non_token_names_unchanged() {
        assert_eq!(canonical_header_name("accept-encoding"), "Accept-Encoding");
        assert_eq!(canonical_header_name("X-Foo_Bar"), "X-Foo_bar");

        for name in ["Foo Bar", "Foo\u{ff}Bar", "Foo:Bar", "Föo"] {
            assert_eq!(
                canonical_header_name(name),
                name,
                "non-token name {name:?} must be returned unchanged"
            );
        }
    }

    #[test]
    fn validate_rejects_nul_byte_in_path() {
        let request = Request::new("GET", b"/foo\0bar".to_vec());
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_rejects_non_numeric_content_length() {
        let request =
            Request::new("POST", b"/".to_vec()).with_header("Content-Length", b"abc".to_vec());
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_rejects_negative_content_length() {
        let request =
            Request::new("POST", b"/".to_vec()).with_header("Content-Length", b"-1".to_vec());
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn go_quote_matches_gos_percent_q_over_byte_strings() {
        // Go: fmt.Sprintf("%q", s) == strconv.Quote(s), over a *byte* string.
        for (raw, want) in [
            (b"".as_slice(), r#""""#),
            (b"5".as_slice(), r#""5""#),
            (b"a b".as_slice(), r#""a b""#),
            // Not valid UTF-8: the byte survives as \xNN rather than being
            // replaced by U+FFFD.
            (b"\xff".as_slice(), r#""\xff""#),
            (b"\x80\xfe".as_slice(), r#""\x80\xfe""#),
            // A truncated 3-byte sequence: Go's DecodeRuneInString reports
            // width 1 for any invalid encoding, so this is two escapes, not
            // one replacement character.
            (b"\xe4\xb8".as_slice(), r#""\xe4\xb8""#),
            (b"1\x002".as_slice(), r#""1\x002""#),
            (b"a\tb\nc\r".as_slice(), r#""a\tb\nc\r""#),
            (b"\x07\x08\x0c\x0b".as_slice(), r#""\a\b\f\v""#),
            (b"\x1b".as_slice(), r#""\x1b""#),
            (b"\x7f".as_slice(), r#""\x7f""#),
            (b"say \"hi\"\\".as_slice(), r#""say \"hi\"\\""#),
            // Printable non-ASCII stays literal, as Go's IsPrint has it:
            // Nd, Ll and So respectively.
            ("\u{663}".as_bytes(), "\"\u{663}\""),
            ("\u{e9}".as_bytes(), "\"\u{e9}\""),
            ("\u{1f600}".as_bytes(), "\"\u{1f600}\""),
            // Non-printable: \u with four hex digits below 0x10000 (U+0080,
            // Cc), \U with eight above it (U+E0001, Cf).
            ("\u{80}".as_bytes(), r#""\u0080""#),
            ("\u{e0001}".as_bytes(), r#""\U000e0001""#),
            // Grapheme-extended, but printable to Go: U+0301 combining acute
            // (Mn), U+09BE Bengali vowel sign aa (Mc, Other_Grapheme_Extend),
            // U+FF9E halfwidth katakana voiced sound mark (Lm,
            // Other_Grapheme_Extend) and U+20E3 combining enclosing keycap
            // (Me). `char::escape_debug` escapes all four as \uXXXX; Go emits
            // all four as themselves, and so must we. Regression test for the
            // printability oracle in quote_rune -- U+0301 is the case the
            // reviewer of this change found, and the other three are the rest
            // of the family it belongs to.
            ("\u{301}".as_bytes(), "\"\u{301}\""),
            ("\u{9be}".as_bytes(), "\"\u{9be}\""),
            ("\u{ff9e}".as_bytes(), "\"\u{ff9e}\""),
            ("\u{20e3}".as_bytes(), "\"\u{20e3}\""),
            // ...and a combining mark reached through the byte path, since
            // that is how a header value actually arrives: the two UTF-8 bytes
            // cc 81 stay one U+0301 attached to the `e`, and nothing about the
            // pair is escaped.
            (b"e\xcc\x81".as_slice(), "\"e\u{301}\""),
            // Still non-printable despite living next door: U+200C (Cf) is
            // Other_Grapheme_Extend too, and Go escapes it. Guards against
            // "fixing" the above by calling every grapheme-extended scalar
            // printable.
            ("\u{200c}".as_bytes(), r#""\u200c""#),
            ("\u{a0}".as_bytes(), r#""\u00a0""#),
            ("\u{2028}".as_bytes(), r#""\u2028""#),
            // `'` is printable to Go's %q on a string -- unlike Rust's
            // escape_debug, which backslashes it. quote_rune must decide it on
            // the ASCII branch, before the oracle is consulted.
            (b"it's".as_slice(), r#""it's""#),
        ] {
            assert_eq!(go_quote(raw), want, "go_quote({raw:?})");
        }
    }

    #[test]
    fn invalid_content_length_message_keeps_non_utf8_bytes() {
        // Upstream builds this with fmt.Errorf("%w: %q", ...) over the raw
        // header value (context.go:160), so a byte that is not valid UTF-8
        // reaches the client as \xff -- not as the U+FFFD a lossy conversion
        // would substitute, which destroys the one piece of information the
        // message exists to carry.
        let request = Request::new("POST", b"/".to_vec()).with_header("Content-Length", vec![0xff]);
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, r#"invalid Content-Length header: "\xff""#);

        let ascii =
            Request::new("POST", b"/".to_vec()).with_header("Content-Length", b"abc".to_vec());
        assert_eq!(
            validate_request(&ascii).unwrap_err().message,
            r#"invalid Content-Length header: "abc""#
        );
    }

    #[test]
    fn validate_accepts_duplicate_valid_content_length_headers() {
        // Header.Get reads "5", the first value -- joining the two into "5, 5"
        // would fail to parse and turn a request upstream accepts into a 400.
        let request = Request::new("POST", b"/".to_vec())
            .with_header("Content-Length", b"5".to_vec())
            .with_header("Content-Length", b"5".to_vec());
        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn validate_accepts_no_content_length_and_a_valid_one() {
        assert!(validate_request(&Request::new("GET", b"/".to_vec())).is_ok());
        assert!(validate_request(
            &Request::new("POST", b"/".to_vec()).with_header("Content-Length", b"42".to_vec())
        )
        .is_ok());
    }

    #[test]
    fn content_length_is_the_parsed_length_not_the_header() {
        // A chunked body: Go's transport reports -1, which upstream copies
        // verbatim into sapi_request_info (cgi.go:304) and which no
        // Content-Length header value can express. Recovering it from
        // `headers` is therefore impossible -- hence the separate field.
        let chunked = Request::new("POST", b"/".to_vec())
            .with_header("Transfer-Encoding", b"chunked".to_vec())
            .with_content_length(-1);
        assert_eq!(chunked.content_length, -1);
        assert_eq!(chunked.headers.get_first("Content-Length"), None);
        assert!(
            validate_request(&chunked).is_ok(),
            "validate() reads the header (context.go:157), and there is none \
             to reject -- it must not reject the parsed -1 as negative"
        );

        // The two are independent in the other direction too: upstream reads
        // the header for $_SERVER['CONTENT_LENGTH'] (cgi.go:92) and the parsed
        // value for the SAPI, and nothing derives one from the other.
        let sized =
            Request::new("POST", b"/".to_vec()).with_header("Content-Length", b"5".to_vec());
        assert_eq!(sized.content_length, 0);
        assert_eq!(
            sized.with_content_length(5).content_length,
            5,
            "whoever builds the Request fills both in"
        );
    }

    #[test]
    fn request_context_validate_delegates_to_the_request() {
        let ctx = test_context(Some(Request::new("GET", b"/foo\0bar".to_vec())));
        assert_eq!(ctx.validate().unwrap_err().status, 400);

        let no_request = test_context(None);
        assert!(
            no_request.validate().is_ok(),
            "a context with no request has nothing to reject"
        );
    }

    #[test]
    fn request_uri_is_the_raw_target_verbatim_never_reconstructed() {
        // Percent-escapes and a bare trailing '?' cannot survive a decode;
        // request_uri must come from raw_target, not from path + query.
        let request = Request::new("GET", b"/index.php/extra".to_vec())
            .with_raw_target(b"/index.php%2Fextra?".to_vec())
            .with_query(b"".to_vec());
        let ctx = test_context(Some(request));
        assert_eq!(ctx.request_uri, b"/index.php%2Fextra?");
    }

    #[test]
    fn request_uri_is_empty_without_a_raw_target_or_a_request() {
        let ctx = test_context(Some(Request::new("GET", b"/".to_vec())));
        assert_eq!(
            ctx.request_uri, b"",
            "raw_target defaults to empty; request_uri must not silently fall back \
             to reconstructing from path + query"
        );

        let no_request = test_context(None);
        assert_eq!(no_request.request_uri, b"");
    }

    #[test]
    fn arena_pointers_stay_valid_across_spine_reallocation() {
        let mut arena = RequestArena::default();
        let first = arena.alloc(b"hello");

        // Push enough further entries to force the arena's own Vec<Box<[u8]>>
        // spine to reallocate (likely many times over).
        for i in 0..10_000 {
            arena.alloc(format!("entry-{i}").as_bytes());
        }

        // SAFETY: `arena` is still alive and owns `first`'s backing buffer;
        // RequestArena::alloc documents that a returned pointer stays valid
        // across later `alloc` calls, because growing the spine moves the
        // `Box` handles, never the heap bytes they point at.
        let bytes = unsafe { std::ffi::CStr::from_ptr(first) };
        assert_eq!(bytes.to_bytes(), b"hello");
    }

    #[test]
    fn arena_pointers_are_writable_not_just_readable() {
        let mut arena = RequestArena::default();
        let first = arena.alloc(b"hello");

        // The five `sapi_request_info` fields these pointers feed are `char *`,
        // not `const char *`, so C is entitled to write through them. This is
        // the probe that catches a pointer carrying read-only provenance --
        // `Arc::as_ptr() as *mut c_char`, say, which keeps the address and
        // loses the permission. Such a write is UB rather than a crash, so it
        // passes under rustc and fails under Miri; this test exists to be run
        // by `cargo +nightly miri test -p frankenrust-core`.
        //
        // SAFETY: `first` points at a live, uniquely-owned 6-byte buffer of
        // `arena`'s ("hello" plus alloc's trailing NUL). No other pointer into
        // it exists, and `arena` outlives every write below.
        unsafe { first.write(b'H' as c_char) };

        // Force the arena's own Vec<Box<[u8]>> spine to reallocate, repeatedly.
        for i in 0..2_000 {
            arena.alloc(format!("entry-{i}").as_bytes());
        }

        // SAFETY: as above, plus alloc's documented guarantee that growing the
        // spine moves the `Box` handles and never the bytes behind them, so
        // `first` still points at the same live buffer. Offset 4 is its last
        // non-NUL byte.
        unsafe { first.add(4).write(b'O' as c_char) };

        // SAFETY: same buffer, still NUL-terminated at offset 5.
        let round_tripped = unsafe { std::ffi::CStr::from_ptr(first) };
        assert_eq!(
            round_tripped.to_bytes(),
            b"HellO",
            "both writes must have landed in the same still-valid buffer"
        );
    }

    #[test]
    fn close_context_fires_the_completion_signal_exactly_once() {
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&fired);
        let mut ctx = RequestContext::new(
            String::new(),
            Vec::new(),
            Some(Request::new("GET", b"/index.php".to_vec())),
            CompletionSignal::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );

        assert_eq!(fired.load(Ordering::SeqCst), 0);
        ctx.close_context();
        assert_eq!(fired.load(Ordering::SeqCst), 1);

        ctx.close_context();
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "close_context must return early once is_done, and must not fire twice"
        );
    }

    #[test]
    fn close_context_snapshots_client_has_closed_into_client_had_closed() {
        let request = Request::new("GET", b"/index.php".to_vec());
        let cancelled = Arc::clone(&request.cancelled);
        let mut ctx = test_context(Some(request));

        assert!(!ctx.client_has_closed());
        ctx.close_context();
        assert!(
            !ctx.client_had_closed,
            "sanity: an untouched cancellation flag must not read as closed"
        );

        // A second request context, cancelled before close_context runs --
        // client_had_closed must capture that snapshot.
        let request = Request::new("GET", b"/index.php".to_vec());
        let cancelled_before_close = Arc::clone(&request.cancelled);
        cancelled_before_close.store(true, Ordering::SeqCst);
        let mut cancelled_ctx = test_context(Some(request));
        assert!(cancelled_ctx.client_has_closed());
        cancelled_ctx.close_context();
        assert!(cancelled_ctx.client_had_closed);

        // cancelled is only read, never written, by this module.
        assert!(!cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn a_completion_signal_may_consume_what_it_captures() {
        // The documented async end is `oneshot::Sender::send(self, ..)`,
        // which consumes the sender. This only type-checks if the signal is
        // FnOnce, which is as much what this test proves as the assertion.
        let (tx, rx) = std::sync::mpsc::channel();
        let payload = "response ready".to_string();
        let mut ctx = RequestContext::new(
            String::new(),
            Vec::new(),
            Some(Request::new("GET", b"/index.php".to_vec())),
            CompletionSignal::new(move || {
                let _ = tx.send(payload);
            }),
        );

        ctx.close_context();
        assert_eq!(rx.try_recv().unwrap(), "response ready");
    }

    #[test]
    fn drop_releases_arena_but_is_done_does_not() {
        let request = Request::new("GET", b"/".to_vec());
        // A live `Weak` here means the `RequestContext` itself is still
        // alive: it owns `request`, which owns this `Arc`. The arena is a
        // plain owned field of the same struct, so the two are released at
        // exactly the same moment -- which is the property this test is
        // about, the arena's *reclaim point*, not whether `Box` frees.
        let context_alive = Arc::downgrade(&request.cancelled);
        let mut ctx = test_context(Some(request));
        let buffer = ctx.arena.alloc(b"hello");

        ctx.close_context();
        assert!(ctx.is_done);
        assert!(
            context_alive.upgrade().is_some(),
            "marking is_done must not drop the context"
        );
        // SAFETY: `ctx` still owns the arena `buffer` came from, and
        // close_context documents that it does not touch the arena; nothing
        // has freed it, so this reads back the bytes alloc copied in.
        let still_there = unsafe { std::ffi::CStr::from_ptr(buffer) };
        assert_eq!(
            still_there.to_bytes(),
            b"hello",
            "marking is_done must not release the arena -- a worker script keeps \
             writing after fastcgi_finish_request()"
        );

        drop(ctx);
        assert!(
            context_alive.upgrade().is_none(),
            "dropping the RequestContext must release everything it owns, its \
             arena included"
        );
    }

    #[test]
    fn context_slots_set_get_clear_round_trip() {
        let slots = ContextSlots::new();
        assert!(slots.with_context(0, |ctx| ctx.is_none()));

        slots.set(0, test_context(None));
        assert!(slots.with_context(0, |ctx| ctx.is_some()));

        // A second thread_index must not disturb the first, and growth must
        // not lose what was already there.
        slots.set(5, test_context(None));
        assert!(slots.with_context(0, |ctx| ctx.is_some()));
        assert!(slots.with_context(5, |ctx| ctx.is_some()));

        slots.clear(0);
        assert!(slots.with_context(0, |ctx| ctx.is_none()));
        assert!(slots.with_context(5, |ctx| ctx.is_some()));
    }

    #[test]
    fn context_slots_set_replaces_and_drops_previous() {
        let slots = ContextSlots::new();
        let request = Request::new("GET", b"/".to_vec());
        // As in drop_releases_arena_but_is_done_does_not: this Weak tracks the
        // whole context, arena included.
        let first_alive = Arc::downgrade(&request.cancelled);
        let mut first = test_context(Some(request));
        first.arena.alloc(b"first");
        slots.set(2, first);
        assert!(first_alive.upgrade().is_some(), "sanity: still in the slot");

        slots.set(2, test_context(None));
        assert!(
            first_alive.upgrade().is_none(),
            "set() must drop (and so release the arena of) the previous context"
        );

        slots.clear(2);
        let request = Request::new("GET", b"/".to_vec());
        let second_alive = Arc::downgrade(&request.cancelled);
        slots.set(2, test_context(Some(request)));
        slots.clear(2);
        assert!(
            second_alive.upgrade().is_none(),
            "clear() must drop (and so release the arena of) the context too"
        );
    }

    #[test]
    fn a_poisoned_slot_is_recovered_rather_than_deadlocking_or_aborting() {
        let slots = ContextSlots::new();
        slots.set(3, test_context(None));

        // The only way to poison a Mutex: panic while holding its guard.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slots.with_context_mut(3, |_| panic!("poison slot 3"));
        }));
        assert!(result.is_err(), "sanity: the closure must panic");

        // The poisoned slot is still readable, writable and clearable.
        assert!(slots.with_context(3, |ctx| ctx.is_some()));
        slots.set(3, test_context(None));
        slots.clear(3);
        assert!(slots.with_context(3, |ctx| ctx.is_none()));
    }

    #[test]
    fn nested_access_to_a_different_slot_panics_instead_of_deadlocking() {
        // Superseded design: an earlier version of this module claimed
        // reaching a *different* slot from inside a with_context closure was
        // safe (it is not -- see the ABBA case below), and shipped a test
        // asserting exactly that nested-access pattern succeeded. That test
        // was wrong to pass, not merely weak: it exercised a genuinely
        // unsafe API shape. This test asserts the corrected behaviour --
        // SingleSlotGuard turns the same call pattern into an immediate
        // panic on this one thread, well before two threads doing it in
        // opposite orders could ever deadlock.
        let slots = ContextSlots::new();
        slots.set(0, test_context(None));
        slots.set(1, test_context(None));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slots.with_context(0, |ctx| {
                assert!(ctx.is_some());
                slots.with_context(1, |other| other.is_some())
            });
        }));
        assert!(
            result.is_err(),
            "nesting with_context(1, ..) inside with_context(0, ..) must panic, not succeed"
        );

        // The panic must not leave this thread permanently locked out: both
        // slots are independently usable again afterwards.
        assert!(slots.with_context(0, |ctx| ctx.is_some()));
        assert!(slots.with_context(1, |ctx| ctx.is_some()));
    }

    #[test]
    fn nested_set_from_within_a_closure_panics_even_for_a_new_index() {
        // Growing the table (via `set` on a never-seen index) from inside
        // another slot's closure is the same hazard as reaching an
        // already-populated slot: `set` goes through the same
        // SingleSlotGuard-gated entry point, so nesting it panics too.
        let slots = ContextSlots::new();
        slots.set(0, test_context(None));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slots.with_context_mut(0, |ctx| {
                ctx.expect("slot 0 was set").arena.alloc(b"x");
                slots.set(4096, test_context(None));
            });
        }));
        assert!(
            result.is_err(),
            "nesting set(4096, ..) inside with_context_mut(0, ..) must panic, not grow the table"
        );
        assert!(
            slots.with_context(4096, |ctx| ctx.is_none()),
            "the panicked set() must not have installed a context"
        );
    }

    #[test]
    fn opposite_order_cross_thread_nesting_panics_on_both_threads_rather_than_deadlocking() {
        // The exact scenario a reviewer used to block the previous version
        // of this module: thread A holds slot 0 and reaches for slot 1 while
        // thread B holds slot 1 and reaches for slot 0. Wrapped in
        // run_or_fail_on_deadlock as a safety net -- if this regresses, the
        // test must fail loudly within 10s rather than hang the gate.
        let slots = Arc::new(ContextSlots::new());
        slots.set(0, test_context(None));
        slots.set(1, test_context(None));

        let slots_for_body = Arc::clone(&slots);
        run_or_fail_on_deadlock("opposite-order cross-slot nesting", move || {
            let slots = slots_for_body;
            let barrier = Arc::new(std::sync::Barrier::new(2));

            let threads: Vec<_> = [(0usize, 1usize), (1, 0)]
                .into_iter()
                .map(|(held, wanted)| {
                    let slots = Arc::clone(&slots);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            slots.with_context(held, |_| {
                                barrier.wait();
                                // Give the other thread a moment to be inside
                                // its own outer with_context call too, so both
                                // sides are genuinely holding a lock when each
                                // reaches for the other's.
                                std::thread::sleep(std::time::Duration::from_millis(20));
                                slots.with_context(wanted, |_| {});
                            });
                        }))
                    })
                })
                .collect();

            let mut panicked = 0;
            for handle in threads {
                if handle
                    .join()
                    .expect("thread must not itself panic across join")
                    .is_err()
                {
                    panicked += 1;
                }
            }
            assert_eq!(
                panicked, 2,
                "both threads must panic via SingleSlotGuard rather than deadlock"
            );
        });

        // Both slots must be usable again afterwards.
        assert!(slots.with_context(0, |ctx| ctx.is_some()));
        assert!(slots.with_context(1, |ctx| ctx.is_some()));
    }

    #[test]
    fn context_slots_concurrent_access_neither_deadlocks_nor_loses_a_slot() {
        let slots = Arc::new(ContextSlots::new());

        // Distinct indices, hammered concurrently: exercises the growth path
        // in `slot()` racing across threads. If a slot were ever lost, the
        // final assertion loop below would find it missing.
        let handles: Vec<_> = (0..16usize)
            .map(|i| {
                let slots = Arc::clone(&slots);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        slots.set(i, test_context(None));
                        slots.with_context(i, |ctx| assert!(ctx.is_some()));
                        slots.clear(i);
                    }
                    slots.set(i, test_context(None));
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread must not panic");
        }
        for i in 0..16usize {
            assert!(
                slots.with_context(i, |ctx| ctx.is_some()),
                "thread {i}'s slot must have survived concurrent access"
            );
        }

        // The same index, hammered from many threads at once: exercises the
        // per-slot Mutex under real contention rather than each thread
        // owning its own index.
        let shared_handles: Vec<_> = (0..16usize)
            .map(|_| {
                let slots = Arc::clone(&slots);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        slots.set(9, test_context(None));
                        slots.with_context_mut(9, |ctx| {
                            ctx.expect("a context was just set").arena.alloc(b"x");
                        });
                    }
                })
            })
            .collect();
        for handle in shared_handles {
            handle.join().expect("worker thread must not panic");
        }
        assert!(slots.with_context(9, |ctx| ctx.is_some()));
    }
}
