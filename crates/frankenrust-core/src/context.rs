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
//!   a separate module's job. "Exactly as given" includes *not* defaulting an
//!   absent `split_path` to `[".php"]` -- see [`RequestContext::split_path`],
//!   where absent and explicitly-empty are different configurations.
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
    /// Go's method yields `URL.EscapedPath()`, then `?` and the raw query
    /// whenever there is a query *or* the client sent a bare `?`
    /// (`URL.ForceQuery`), and `/` when the escaped path is empty. It carries
    /// **no scheme, no authority and no fragment** -- even when the client
    /// sent an absolute-form target, `GET http://example.com/a?b=1 HTTP/1.1`,
    /// which RFC 9110 §7.1 allows and proxies do send. So a server layer must
    /// never fill this from a whole-URI string: that keeps the scheme and
    /// authority of an absolute-form target, and this field reaches PHP as
    /// `$_SERVER['REQUEST_URI']`, where the difference breaks every router
    /// matching on a path.
    ///
    /// **`EscapedPath()` is not simply the wire path**, and a server layer
    /// that fills this from the raw target slice (`http`'s
    /// `Uri::path_and_query()`, say) is *not* faithful to upstream. Go returns
    /// the wire path verbatim only when it round-trips -- when
    /// `validEncoded(RawPath, encodePath)` holds *and* unescaping it
    /// reproduces `URL.Path`. Otherwise it returns `escape(URL.Path,
    /// encodePath)`: a canonical re-encoding computed from the **decoded**
    /// path (`$(go env GOROOT)/src/net/url/url.go`). Measured against
    /// go1.26.4's `http.ReadRequest`, that fallback fires for any path byte
    /// `encodePath` would escape -- raw UTF-8 (`/café` -> `/caf%C3%A9`, which
    /// curl and most non-browser clients send unencoded), and `"` `\` `^` `|`
    /// `{` `}`. When such a byte shares a path with an escape, the escape is
    /// decoded too: wire `/%2f"` -> `//%22` (the `%2f` becomes a *structural*
    /// slash) and wire `/%41"b` -> `/A%22b`.
    ///
    /// So: percent escapes survive unmodified (`/%2f` -> `/%2f`, `/%41` ->
    /// `/%41`) only while the whole path passes `validEncoded`; the bare
    /// trailing `?` survives unconditionally, since `ForceQuery` is appended
    /// after the path either way. Neither can be recovered from
    /// [`Request::path`] once it has been decoded, which is why this is a
    /// field of its own -- but "not recoverable from `path`" is a weaker
    /// property than "identical to the wire bytes", and only the first one
    /// holds. Porting `EscapedPath()`'s round-trip check and re-escape
    /// fallback is the server layer's job, tracked as its own issue; this
    /// module only ever copies whatever it is handed.
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
    /// wire owns computing this the way `URL.RequestURI()` does -- which is
    /// *not* the literal request-target bytes whenever the wire path fails
    /// `EscapedPath()`'s round-trip check.
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
/// # Byte-exactness
///
/// Printability is [`go_is_print`] -- Go's own generated table, not rustc's,
/// for the reason written down there. That leaves `go_quote` byte-identical to
/// `strconv.Quote` with no residue over the 2,424,384-string corpus it is
/// tested against: every single byte, every one of the 1,112,064 Unicode
/// scalar values alone and embedded, and 200k pseudo-random byte soups. See
/// `go_quote_matches_go_1_26_over_a_generated_corpus`.
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

    // `appendEscapedRune`'s only printability test once `ASCIIonly` and
    // `graphicOnly` are false, which is what `%q` on a string passes: a single
    // `IsPrint(r)` covering ASCII and non-ASCII alike.
    if go_is_print(c) {
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

/// `strconv.IsPrint` (`strconv/quote.go`), the predicate `appendEscapedRune`
/// consults, ported together with the generated tables it reads.
///
/// # Why Go's tables are carried instead of delegating to rustc
///
/// `str::escape_debug` looks like the same predicate -- both are "categories
/// L, M, N, P, S, plus ASCII space" -- and an earlier revision of this file
/// used it, in non-leading position to suppress Rust's extra
/// grapheme-extended rule. It is not the same predicate, because the two
/// toolchains generate their tables from **different Unicode versions**:
/// `strconv`'s come from the `unicode` package of the Go release (15.0.0 at
/// go1.26, `unicode/tables.go`), rustc's from whichever version that rustc
/// shipped. Every scalar assigned in between is a disagreement, and there were
/// 10,615 of them between go1.26 and rustc 1.97.1 -- every one measured Cn
/// (unassigned) in go1.26's tables and printable to rustc. U+2EBF0 (CJK Ext I,
/// assigned in Unicode 15.1) is one: upstream renders a `Content-Length`
/// carrying it as `"\U0002ebf0"`, where delegating rendered the ideograph
/// itself. (Three further scalars differ in the other direction -- `"`, `'`
/// and `\`, which `escape_debug` backslashes and Go prints. Those are not
/// version skew, and the old code sidestepped them with a separate ASCII
/// branch that this function's Latin-1 fast path now subsumes.)
///
/// Delegating cannot be repaired, because the disagreement is not a bug in
/// either table: it is two independent Unicode revisions, and it *moves*
/// whenever either toolchain updates -- in rustc's case underneath us, on a
/// dependency bump nobody would connect to a 400's wording. Carrying Go's
/// tables takes rustc out of the oracle altogether. The only version this
/// tracks is the one `vendor/frankenphp/go.mod` pins (`go 1.26.0`), and it can
/// only change when that line does.
///
/// The four tables are `strconv/isprint.go` transcribed verbatim (reflowed;
/// that file is itself generated, by `go run makeisprint.go`, so this is data
/// rather than a re-derivation of the category rules). To re-check them after
/// a Go bump, diff the numbers against
/// `"$(go env GOROOT)/src/strconv/isprint.go"` -- and note that
/// `go_is_print_matches_go_1_26_over_every_scalar` pins all 1,112,064 results,
/// so a single mistyped digit fails the gate rather than one obscure request.
/// `isGraphic` is deliberately not carried: it is read only when `graphicOnly`
/// is set, and `%q` on a string never sets it.
fn go_is_print(c: char) -> bool {
    let r = c as u32;

    // Fast check for Latin-1, soft-hyphen hole and all.
    if r <= 0xff {
        if (0x20..=0x7e).contains(&r) {
            return true;
        }
        if (0xa1..=0xff).contains(&r) {
            return r != 0xad;
        }
        return false;
    }

    // Go's `bsearch` returns the first index whose entry is >= the needle,
    // which is `partition_point`'s definition exactly; its second return value
    // (an exact hit) is `binary_search(..).is_ok()`. The print tables are flat
    // lists of inclusive `[lo, hi]` pairs, so for the candidate index `i`,
    // `i & !1` is the pair's `lo` and `i | 1` its `hi` -- both in bounds once
    // `i < len`, because the lengths are even (asserted below).
    if r < 0x1_0000 {
        let needle = r as u16;
        let i = GO_IS_PRINT_16.partition_point(|&e| e < needle);
        if i >= GO_IS_PRINT_16.len()
            || needle < GO_IS_PRINT_16[i & !1]
            || GO_IS_PRINT_16[i | 1] < needle
        {
            return false;
        }
        return GO_IS_NOT_PRINT_16.binary_search(&needle).is_err();
    }

    let i = GO_IS_PRINT_32.partition_point(|&e| e < r);
    if i >= GO_IS_PRINT_32.len() || r < GO_IS_PRINT_32[i & !1] || GO_IS_PRINT_32[i | 1] < r {
        return false;
    }
    if r >= 0x2_0000 {
        // `isNotPrint32` stores 16-bit offsets from 0x10000, so it cannot
        // describe a hole above plane 1 and Go returns early rather than
        // truncating the needle into a false match.
        return true;
    }
    GO_IS_NOT_PRINT_32
        .binary_search(&((r - 0x1_0000) as u16))
        .is_err()
}

// `go_is_print` indexes `i | 1` after checking only `i < len`, which is in
// bounds precisely because these are pair tables. Checked here so that a
// mis-transcription that drops one entry is a compile error rather than a
// panic on some unlucky header value.
const _: () = assert!(GO_IS_PRINT_16.len().is_multiple_of(2));
const _: () = assert!(GO_IS_PRINT_32.len().is_multiple_of(2));

const GO_IS_PRINT_16: [u16; 424] = [
    0x0020, 0x007e, 0x00a1, 0x0377, 0x037a, 0x037f, 0x0384, 0x0556, 0x0559, 0x058a, 0x058d, 0x05c7,
    0x05d0, 0x05ea, 0x05ef, 0x05f4, 0x0606, 0x070d, 0x0710, 0x074a, 0x074d, 0x07b1, 0x07c0, 0x07fa,
    0x07fd, 0x082d, 0x0830, 0x085b, 0x085e, 0x086a, 0x0870, 0x088e, 0x0898, 0x098c, 0x098f, 0x0990,
    0x0993, 0x09b2, 0x09b6, 0x09b9, 0x09bc, 0x09c4, 0x09c7, 0x09c8, 0x09cb, 0x09ce, 0x09d7, 0x09d7,
    0x09dc, 0x09e3, 0x09e6, 0x09fe, 0x0a01, 0x0a0a, 0x0a0f, 0x0a10, 0x0a13, 0x0a39, 0x0a3c, 0x0a42,
    0x0a47, 0x0a48, 0x0a4b, 0x0a4d, 0x0a51, 0x0a51, 0x0a59, 0x0a5e, 0x0a66, 0x0a76, 0x0a81, 0x0ab9,
    0x0abc, 0x0acd, 0x0ad0, 0x0ad0, 0x0ae0, 0x0ae3, 0x0ae6, 0x0af1, 0x0af9, 0x0b0c, 0x0b0f, 0x0b10,
    0x0b13, 0x0b39, 0x0b3c, 0x0b44, 0x0b47, 0x0b48, 0x0b4b, 0x0b4d, 0x0b55, 0x0b57, 0x0b5c, 0x0b63,
    0x0b66, 0x0b77, 0x0b82, 0x0b8a, 0x0b8e, 0x0b95, 0x0b99, 0x0b9f, 0x0ba3, 0x0ba4, 0x0ba8, 0x0baa,
    0x0bae, 0x0bb9, 0x0bbe, 0x0bc2, 0x0bc6, 0x0bcd, 0x0bd0, 0x0bd0, 0x0bd7, 0x0bd7, 0x0be6, 0x0bfa,
    0x0c00, 0x0c39, 0x0c3c, 0x0c4d, 0x0c55, 0x0c5a, 0x0c5d, 0x0c5d, 0x0c60, 0x0c63, 0x0c66, 0x0c6f,
    0x0c77, 0x0cb9, 0x0cbc, 0x0ccd, 0x0cd5, 0x0cd6, 0x0cdd, 0x0ce3, 0x0ce6, 0x0cf3, 0x0d00, 0x0d4f,
    0x0d54, 0x0d63, 0x0d66, 0x0d96, 0x0d9a, 0x0dbd, 0x0dc0, 0x0dc6, 0x0dca, 0x0dca, 0x0dcf, 0x0ddf,
    0x0de6, 0x0def, 0x0df2, 0x0df4, 0x0e01, 0x0e3a, 0x0e3f, 0x0e5b, 0x0e81, 0x0ebd, 0x0ec0, 0x0ed9,
    0x0edc, 0x0edf, 0x0f00, 0x0f6c, 0x0f71, 0x0fda, 0x1000, 0x10c7, 0x10cd, 0x10cd, 0x10d0, 0x124d,
    0x1250, 0x125d, 0x1260, 0x128d, 0x1290, 0x12b5, 0x12b8, 0x12c5, 0x12c8, 0x1315, 0x1318, 0x135a,
    0x135d, 0x137c, 0x1380, 0x1399, 0x13a0, 0x13f5, 0x13f8, 0x13fd, 0x1400, 0x169c, 0x16a0, 0x16f8,
    0x1700, 0x1715, 0x171f, 0x1736, 0x1740, 0x1753, 0x1760, 0x1773, 0x1780, 0x17dd, 0x17e0, 0x17e9,
    0x17f0, 0x17f9, 0x1800, 0x1819, 0x1820, 0x1878, 0x1880, 0x18aa, 0x18b0, 0x18f5, 0x1900, 0x192b,
    0x1930, 0x193b, 0x1940, 0x1940, 0x1944, 0x196d, 0x1970, 0x1974, 0x1980, 0x19ab, 0x19b0, 0x19c9,
    0x19d0, 0x19da, 0x19de, 0x1a1b, 0x1a1e, 0x1a7c, 0x1a7f, 0x1a89, 0x1a90, 0x1a99, 0x1aa0, 0x1aad,
    0x1ab0, 0x1ace, 0x1b00, 0x1b4c, 0x1b50, 0x1bf3, 0x1bfc, 0x1c37, 0x1c3b, 0x1c49, 0x1c4d, 0x1c88,
    0x1c90, 0x1cba, 0x1cbd, 0x1cc7, 0x1cd0, 0x1cfa, 0x1d00, 0x1f15, 0x1f18, 0x1f1d, 0x1f20, 0x1f45,
    0x1f48, 0x1f4d, 0x1f50, 0x1f7d, 0x1f80, 0x1fd3, 0x1fd6, 0x1fef, 0x1ff2, 0x1ffe, 0x2010, 0x2027,
    0x2030, 0x205e, 0x2070, 0x2071, 0x2074, 0x209c, 0x20a0, 0x20c0, 0x20d0, 0x20f0, 0x2100, 0x218b,
    0x2190, 0x2426, 0x2440, 0x244a, 0x2460, 0x2b73, 0x2b76, 0x2cf3, 0x2cf9, 0x2d27, 0x2d2d, 0x2d2d,
    0x2d30, 0x2d67, 0x2d6f, 0x2d70, 0x2d7f, 0x2d96, 0x2da0, 0x2e5d, 0x2e80, 0x2ef3, 0x2f00, 0x2fd5,
    0x2ff0, 0x2ffb, 0x3001, 0x3096, 0x3099, 0x30ff, 0x3105, 0x31e3, 0x31f0, 0xa48c, 0xa490, 0xa4c6,
    0xa4d0, 0xa62b, 0xa640, 0xa6f7, 0xa700, 0xa7ca, 0xa7d0, 0xa7d9, 0xa7f2, 0xa82c, 0xa830, 0xa839,
    0xa840, 0xa877, 0xa880, 0xa8c5, 0xa8ce, 0xa8d9, 0xa8e0, 0xa953, 0xa95f, 0xa97c, 0xa980, 0xa9d9,
    0xa9de, 0xaa36, 0xaa40, 0xaa4d, 0xaa50, 0xaa59, 0xaa5c, 0xaac2, 0xaadb, 0xaaf6, 0xab01, 0xab06,
    0xab09, 0xab0e, 0xab11, 0xab16, 0xab20, 0xab6b, 0xab70, 0xabed, 0xabf0, 0xabf9, 0xac00, 0xd7a3,
    0xd7b0, 0xd7c6, 0xd7cb, 0xd7fb, 0xf900, 0xfa6d, 0xfa70, 0xfad9, 0xfb00, 0xfb06, 0xfb13, 0xfb17,
    0xfb1d, 0xfbc2, 0xfbd3, 0xfd8f, 0xfd92, 0xfdc7, 0xfdcf, 0xfdcf, 0xfdf0, 0xfe19, 0xfe20, 0xfe6b,
    0xfe70, 0xfefc, 0xff01, 0xffbe, 0xffc2, 0xffc7, 0xffca, 0xffcf, 0xffd2, 0xffd7, 0xffda, 0xffdc,
    0xffe0, 0xffee, 0xfffc, 0xfffd,
];

const GO_IS_NOT_PRINT_16: [u16; 133] = [
    0x00ad, 0x038b, 0x038d, 0x03a2, 0x0530, 0x0590, 0x061c, 0x06dd, 0x083f, 0x085f, 0x08e2, 0x0984,
    0x09a9, 0x09b1, 0x09de, 0x0a04, 0x0a29, 0x0a31, 0x0a34, 0x0a37, 0x0a3d, 0x0a5d, 0x0a84, 0x0a8e,
    0x0a92, 0x0aa9, 0x0ab1, 0x0ab4, 0x0ac6, 0x0aca, 0x0b00, 0x0b04, 0x0b29, 0x0b31, 0x0b34, 0x0b5e,
    0x0b84, 0x0b91, 0x0b9b, 0x0b9d, 0x0bc9, 0x0c0d, 0x0c11, 0x0c29, 0x0c45, 0x0c49, 0x0c57, 0x0c8d,
    0x0c91, 0x0ca9, 0x0cb4, 0x0cc5, 0x0cc9, 0x0cdf, 0x0cf0, 0x0d0d, 0x0d11, 0x0d45, 0x0d49, 0x0d80,
    0x0d84, 0x0db2, 0x0dbc, 0x0dd5, 0x0dd7, 0x0e83, 0x0e85, 0x0e8b, 0x0ea4, 0x0ea6, 0x0ec5, 0x0ec7,
    0x0ecf, 0x0f48, 0x0f98, 0x0fbd, 0x0fcd, 0x10c6, 0x1249, 0x1257, 0x1259, 0x1289, 0x12b1, 0x12bf,
    0x12c1, 0x12d7, 0x1311, 0x1680, 0x176d, 0x1771, 0x180e, 0x191f, 0x1a5f, 0x1b7f, 0x1f58, 0x1f5a,
    0x1f5c, 0x1f5e, 0x1fb5, 0x1fc5, 0x1fdc, 0x1ff5, 0x208f, 0x2b96, 0x2d26, 0x2da7, 0x2daf, 0x2db7,
    0x2dbf, 0x2dc7, 0x2dcf, 0x2dd7, 0x2ddf, 0x2e9a, 0x3040, 0x3130, 0x318f, 0x321f, 0xa7d2, 0xa7d4,
    0xa9ce, 0xa9ff, 0xab27, 0xab2f, 0xfb37, 0xfb3d, 0xfb3f, 0xfb42, 0xfb45, 0xfe53, 0xfe67, 0xfe75,
    0xffe7,
];

const GO_IS_PRINT_32: [u32; 508] = [
    0x010000, 0x01004d, 0x010050, 0x01005d, 0x010080, 0x0100fa, 0x010100, 0x010102, 0x010107,
    0x010133, 0x010137, 0x01019c, 0x0101a0, 0x0101a0, 0x0101d0, 0x0101fd, 0x010280, 0x01029c,
    0x0102a0, 0x0102d0, 0x0102e0, 0x0102fb, 0x010300, 0x010323, 0x01032d, 0x01034a, 0x010350,
    0x01037a, 0x010380, 0x0103c3, 0x0103c8, 0x0103d5, 0x010400, 0x01049d, 0x0104a0, 0x0104a9,
    0x0104b0, 0x0104d3, 0x0104d8, 0x0104fb, 0x010500, 0x010527, 0x010530, 0x010563, 0x01056f,
    0x0105bc, 0x010600, 0x010736, 0x010740, 0x010755, 0x010760, 0x010767, 0x010780, 0x0107ba,
    0x010800, 0x010805, 0x010808, 0x010838, 0x01083c, 0x01083c, 0x01083f, 0x01089e, 0x0108a7,
    0x0108af, 0x0108e0, 0x0108f5, 0x0108fb, 0x01091b, 0x01091f, 0x010939, 0x01093f, 0x01093f,
    0x010980, 0x0109b7, 0x0109bc, 0x0109cf, 0x0109d2, 0x010a06, 0x010a0c, 0x010a35, 0x010a38,
    0x010a3a, 0x010a3f, 0x010a48, 0x010a50, 0x010a58, 0x010a60, 0x010a9f, 0x010ac0, 0x010ae6,
    0x010aeb, 0x010af6, 0x010b00, 0x010b35, 0x010b39, 0x010b55, 0x010b58, 0x010b72, 0x010b78,
    0x010b91, 0x010b99, 0x010b9c, 0x010ba9, 0x010baf, 0x010c00, 0x010c48, 0x010c80, 0x010cb2,
    0x010cc0, 0x010cf2, 0x010cfa, 0x010d27, 0x010d30, 0x010d39, 0x010e60, 0x010ead, 0x010eb0,
    0x010eb1, 0x010efd, 0x010f27, 0x010f30, 0x010f59, 0x010f70, 0x010f89, 0x010fb0, 0x010fcb,
    0x010fe0, 0x010ff6, 0x011000, 0x01104d, 0x011052, 0x011075, 0x01107f, 0x0110c2, 0x0110d0,
    0x0110e8, 0x0110f0, 0x0110f9, 0x011100, 0x011147, 0x011150, 0x011176, 0x011180, 0x0111f4,
    0x011200, 0x011241, 0x011280, 0x0112a9, 0x0112b0, 0x0112ea, 0x0112f0, 0x0112f9, 0x011300,
    0x01130c, 0x01130f, 0x011310, 0x011313, 0x011344, 0x011347, 0x011348, 0x01134b, 0x01134d,
    0x011350, 0x011350, 0x011357, 0x011357, 0x01135d, 0x011363, 0x011366, 0x01136c, 0x011370,
    0x011374, 0x011400, 0x011461, 0x011480, 0x0114c7, 0x0114d0, 0x0114d9, 0x011580, 0x0115b5,
    0x0115b8, 0x0115dd, 0x011600, 0x011644, 0x011650, 0x011659, 0x011660, 0x01166c, 0x011680,
    0x0116b9, 0x0116c0, 0x0116c9, 0x011700, 0x01171a, 0x01171d, 0x01172b, 0x011730, 0x011746,
    0x011800, 0x01183b, 0x0118a0, 0x0118f2, 0x0118ff, 0x011906, 0x011909, 0x011909, 0x01190c,
    0x011938, 0x01193b, 0x011946, 0x011950, 0x011959, 0x0119a0, 0x0119a7, 0x0119aa, 0x0119d7,
    0x0119da, 0x0119e4, 0x011a00, 0x011a47, 0x011a50, 0x011aa2, 0x011ab0, 0x011af8, 0x011b00,
    0x011b09, 0x011c00, 0x011c45, 0x011c50, 0x011c6c, 0x011c70, 0x011c8f, 0x011c92, 0x011cb6,
    0x011d00, 0x011d36, 0x011d3a, 0x011d47, 0x011d50, 0x011d59, 0x011d60, 0x011d98, 0x011da0,
    0x011da9, 0x011ee0, 0x011ef8, 0x011f00, 0x011f3a, 0x011f3e, 0x011f59, 0x011fb0, 0x011fb0,
    0x011fc0, 0x011ff1, 0x011fff, 0x012399, 0x012400, 0x012474, 0x012480, 0x012543, 0x012f90,
    0x012ff2, 0x013000, 0x01342f, 0x013440, 0x013455, 0x014400, 0x014646, 0x016800, 0x016a38,
    0x016a40, 0x016a69, 0x016a6e, 0x016ac9, 0x016ad0, 0x016aed, 0x016af0, 0x016af5, 0x016b00,
    0x016b45, 0x016b50, 0x016b77, 0x016b7d, 0x016b8f, 0x016e40, 0x016e9a, 0x016f00, 0x016f4a,
    0x016f4f, 0x016f87, 0x016f8f, 0x016f9f, 0x016fe0, 0x016fe4, 0x016ff0, 0x016ff1, 0x017000,
    0x0187f7, 0x018800, 0x018cd5, 0x018d00, 0x018d08, 0x01aff0, 0x01b122, 0x01b132, 0x01b132,
    0x01b150, 0x01b152, 0x01b155, 0x01b155, 0x01b164, 0x01b167, 0x01b170, 0x01b2fb, 0x01bc00,
    0x01bc6a, 0x01bc70, 0x01bc7c, 0x01bc80, 0x01bc88, 0x01bc90, 0x01bc99, 0x01bc9c, 0x01bc9f,
    0x01cf00, 0x01cf2d, 0x01cf30, 0x01cf46, 0x01cf50, 0x01cfc3, 0x01d000, 0x01d0f5, 0x01d100,
    0x01d126, 0x01d129, 0x01d172, 0x01d17b, 0x01d1ea, 0x01d200, 0x01d245, 0x01d2c0, 0x01d2d3,
    0x01d2e0, 0x01d2f3, 0x01d300, 0x01d356, 0x01d360, 0x01d378, 0x01d400, 0x01d49f, 0x01d4a2,
    0x01d4a2, 0x01d4a5, 0x01d4a6, 0x01d4a9, 0x01d50a, 0x01d50d, 0x01d546, 0x01d54a, 0x01d6a5,
    0x01d6a8, 0x01d7cb, 0x01d7ce, 0x01da8b, 0x01da9b, 0x01daaf, 0x01df00, 0x01df1e, 0x01df25,
    0x01df2a, 0x01e000, 0x01e018, 0x01e01b, 0x01e02a, 0x01e030, 0x01e06d, 0x01e08f, 0x01e08f,
    0x01e100, 0x01e12c, 0x01e130, 0x01e13d, 0x01e140, 0x01e149, 0x01e14e, 0x01e14f, 0x01e290,
    0x01e2ae, 0x01e2c0, 0x01e2f9, 0x01e2ff, 0x01e2ff, 0x01e4d0, 0x01e4f9, 0x01e7e0, 0x01e8c4,
    0x01e8c7, 0x01e8d6, 0x01e900, 0x01e94b, 0x01e950, 0x01e959, 0x01e95e, 0x01e95f, 0x01ec71,
    0x01ecb4, 0x01ed01, 0x01ed3d, 0x01ee00, 0x01ee24, 0x01ee27, 0x01ee3b, 0x01ee42, 0x01ee42,
    0x01ee47, 0x01ee54, 0x01ee57, 0x01ee64, 0x01ee67, 0x01ee9b, 0x01eea1, 0x01eebb, 0x01eef0,
    0x01eef1, 0x01f000, 0x01f02b, 0x01f030, 0x01f093, 0x01f0a0, 0x01f0ae, 0x01f0b1, 0x01f0f5,
    0x01f100, 0x01f1ad, 0x01f1e6, 0x01f202, 0x01f210, 0x01f23b, 0x01f240, 0x01f248, 0x01f250,
    0x01f251, 0x01f260, 0x01f265, 0x01f300, 0x01f6d7, 0x01f6dc, 0x01f6ec, 0x01f6f0, 0x01f6fc,
    0x01f700, 0x01f776, 0x01f77b, 0x01f7d9, 0x01f7e0, 0x01f7eb, 0x01f7f0, 0x01f7f0, 0x01f800,
    0x01f80b, 0x01f810, 0x01f847, 0x01f850, 0x01f859, 0x01f860, 0x01f887, 0x01f890, 0x01f8ad,
    0x01f8b0, 0x01f8b1, 0x01f900, 0x01fa53, 0x01fa60, 0x01fa6d, 0x01fa70, 0x01fa7c, 0x01fa80,
    0x01fa88, 0x01fa90, 0x01fac5, 0x01face, 0x01fadb, 0x01fae0, 0x01fae8, 0x01faf0, 0x01faf8,
    0x01fb00, 0x01fbca, 0x01fbf0, 0x01fbf9, 0x020000, 0x02a6df, 0x02a700, 0x02b739, 0x02b740,
    0x02b81d, 0x02b820, 0x02cea1, 0x02ceb0, 0x02ebe0, 0x02f800, 0x02fa1d, 0x030000, 0x03134a,
    0x031350, 0x0323af, 0x0e0100, 0x0e01ef,
];

const GO_IS_NOT_PRINT_32: [u16; 112] = [
    0x000c, 0x0027, 0x003b, 0x003e, 0x018f, 0x039e, 0x057b, 0x058b, 0x0593, 0x0596, 0x05a2, 0x05b2,
    0x05ba, 0x0786, 0x07b1, 0x0809, 0x0836, 0x0856, 0x08f3, 0x0a04, 0x0a14, 0x0a18, 0x0e7f, 0x0eaa,
    0x10bd, 0x1135, 0x11e0, 0x1212, 0x1287, 0x1289, 0x128e, 0x129e, 0x1304, 0x1329, 0x1331, 0x1334,
    0x133a, 0x145c, 0x1914, 0x1917, 0x1936, 0x1c09, 0x1c37, 0x1ca8, 0x1d07, 0x1d0a, 0x1d3b, 0x1d3e,
    0x1d66, 0x1d69, 0x1d8f, 0x1d92, 0x1f11, 0x246f, 0x6a5f, 0x6abf, 0x6b5a, 0x6b62, 0xaff4, 0xaffc,
    0xafff, 0xd455, 0xd49d, 0xd4ad, 0xd4ba, 0xd4bc, 0xd4c4, 0xd506, 0xd515, 0xd51d, 0xd53a, 0xd53f,
    0xd545, 0xd551, 0xdaa0, 0xe007, 0xe022, 0xe025, 0xe7e7, 0xe7ec, 0xe7ef, 0xe7ff, 0xee04, 0xee20,
    0xee23, 0xee28, 0xee33, 0xee38, 0xee3a, 0xee48, 0xee4a, 0xee4c, 0xee50, 0xee53, 0xee58, 0xee5a,
    0xee5c, 0xee5e, 0xee60, 0xee63, 0xee6b, 0xee73, 0xee78, 0xee7d, 0xee7f, 0xee8a, 0xeea4, 0xeeaa,
    0xf0c0, 0xf0d0, 0xfabe, 0xfb93,
];

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

    /// The CGI split-path list (`context.go:20`), and `Option` because Go's
    /// `[]string` is nullable and `splitCgiPath` branches on exactly that:
    /// `if splitPath == nil { splitPath = []string{".php"} }`
    /// (`cgi.go:195-197`) defaults **only** the nil case, while a non-nil
    /// empty slice falls straight through to `splitPos`, whose own
    /// `if len(splitPath) == 0 { return 0 }` (`cgi.go:239-241`) splits at
    /// offset zero -- an empty `DOCUMENT_URI` and the whole path as
    /// `PATH_INFO`. `WithRequestSplitPath([]string{})` reaches that state
    /// deliberately (`requestoptions.go:86-113` stores the caller's slice
    /// as-is, and `requestoptions_test.go:39` pins `[]` round-tripping to
    /// `[]`), so the two are distinct configurations with different
    /// behaviour, not two spellings of "unset".
    ///
    /// [`None`] is Go's nil -- unconfigured, meaning the CGI layer supplies
    /// `[".php"]`. `Some(vec![])` is the explicit empty list. Collapsing them
    /// into one `Vec<String>` would be unrecoverable: this struct is the only
    /// thing the CGI layer is handed, so whichever of the two behaviours it
    /// then picked would be wrong for the other configuration. Applying the
    /// `[".php"]` default is *not* this module's job -- storing it here would
    /// re-erase the distinction one field later.
    pub split_path: Option<Vec<String>>,

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
    /// verbatim -- **never** rebuilt here from `path` + `query`. A decoded
    /// path cannot represent a percent-escape that `URL.RequestURI()` would
    /// have kept (`/%2f` stays `/%2f` there, but decodes to `//` in `path`),
    /// nor a bare trailing `?` with an empty query (`URL.ForceQuery` -- a
    /// decoded path plus an empty query string is indistinguishable from "no
    /// query at all"). Computing the value that goes in here is the job of
    /// whoever fills [`Request::raw_target`], and that computation is
    /// `URL.RequestURI()`'s, not a slice of the wire bytes: see that field's
    /// doc comment for the re-escape fallback it has to reproduce.
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
    /// resolving an empty document root, and defaulting/validating/normalising
    /// `split_path`, all belong to the module that owns CGI path splitting.
    /// In particular `None` is passed through as `None`: see the field's doc
    /// comment for why substituting `[".php"]` here would be a bug and not a
    /// convenience. `doc_uri`, `path_info`, `script_name` and
    /// `script_filename` start empty for the same reason: computing them from
    /// `request` is that module's job too, not this constructor's.
    pub fn new(
        document_root: String,
        split_path: Option<Vec<String>>,
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
        RequestContext::new(String::new(), None, request, CompletionSignal::none())
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
            // (Me). Go emits all four as themselves, and so must we.
            // Regression test for the printability oracle in quote_rune: an
            // earlier revision delegated to `char::escape_debug`, which
            // escapes all four as \uXXXX. U+0301 is the case a reviewer
            // found; the other three are the rest of the family.
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
            // escape_debug, which backslashes it. Decided by go_is_print's
            // Latin-1 fast path, which is why only `"` and `\` are special
            // cased ahead of it.
            (b"it's".as_slice(), r#""it's""#),
        ] {
            assert_eq!(go_quote(raw), want, "go_quote({raw:?})");
        }
    }

    /// Every Unicode scalar value, in order -- `char::from_u32` drops exactly
    /// the surrogate range, leaving the 1,112,064 the Go oracle iterates.
    fn all_scalars() -> impl Iterator<Item = char> {
        (0..=0x10_FFFF_u32).filter_map(char::from_u32)
    }

    /// FNV-1a, 64-bit: the hash the Go oracle described on
    /// [`go_is_print_matches_go_1_26_over_every_scalar`] computes, so that a
    /// whole-corpus comparison reduces to one constant instead of a data file
    /// this crate would have to carry and keep in sync.
    struct Fnv(u64);

    impl Fnv {
        const PRIME: u64 = 0x100_0000_01b3;

        fn new() -> Self {
            Fnv(0xcbf2_9ce4_8422_2325)
        }

        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= u64::from(b);
                self.0 = self.0.wrapping_mul(Self::PRIME);
            }
        }

        /// A record separator (Go's `sep()`, which is a NUL through the same
        /// step): without it, moving a byte from the end of one output to the
        /// start of the next would not change the digest.
        fn sep(&mut self) {
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// xorshift64. Shifts and xors only -- no addition, no multiplication --
    /// so Go's `uint64` and Rust's `u64` produce identical streams with no
    /// wrapping subtleties, which is what makes the corpus reproducible on
    /// both sides.
    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn go_quote_escapes_the_scalars_go_1_26_calls_unassigned() {
        // The defect this pins. Printability used to be delegated to
        // `char::escape_debug`, i.e. to *rustc's* Unicode tables; upstream's
        // is `strconv.IsPrint`, generated from the `unicode` package of the Go
        // release, which vendor/frankenphp/go.mod pins at go 1.26.0 (Unicode
        // 15.0.0). Every scalar assigned between those two revisions is a
        // disagreement -- 10,615 of them, each printable to rustc and Cn to
        // Go, so each one rendered literally here and escaped upstream.
        //
        // Below: both sides of the gap at every table boundary that moved.
        // U+2EBF0 (CJK Ext I, Unicode 15.1) is the case a reviewer found;
        // U+323B0 is CJK Ext J (Unicode 16.0); U+1FBFA and U+2A6E0 are the
        // same story in other blocks. The `want` strings are the output of
        // `fmt.Sprintf("%q", string(r))` under go1.26.4, not hand-derived.
        for (scalar, want) in [
            // BMP, decided by isPrint16/isNotPrint16.
            (0x0_0377_u32, "\"\u{377}\""),
            (0x0_0378, r#""\u0378""#),
            (0x0_0379, r#""\u0379""#),
            (0x0_037a, "\"\u{37a}\""),
            (0x0_0530, r#""\u0530""#),
            (0x0_0559, "\"\u{559}\""),
            (0x0_fffd, "\"\u{fffd}\""),
            (0x0_fffe, r#""\ufffe""#),
            // Plane 1, where isNotPrint32's 16-bit offsets still apply: U+1000C
            // is a hole punched inside a printable range, its neighbours are
            // not.
            (0x1_0000, "\"\u{10000}\""),
            (0x1_000b, "\"\u{1000b}\""),
            (0x1_000c, r#""\U0001000c""#),
            (0x1_000d, "\"\u{1000d}\""),
            (0x1_fbf9, "\"\u{1fbf9}\""),
            (0x1_fbfa, r#""\U0001fbfa""#),
            // At and above U+20000, where IsPrint returns early on a range hit
            // rather than consulting isNotPrint32.
            (0x2_0000, "\"\u{20000}\""),
            (0x2_a6df, "\"\u{2a6df}\""),
            (0x2_a6e0, r#""\U0002a6e0""#),
            (0x2_ceb0, "\"\u{2ceb0}\""),
            // The range that ends at U+2EBE0 in Unicode 15.0.0 and runs to
            // U+2EE5D in 15.1: printable one scalar earlier, unassigned from
            // there on, for as long as go.mod says 1.26.
            (0x2_ebe0, "\"\u{2ebe0}\""),
            (0x2_ebe1, r#""\U0002ebe1""#),
            (0x2_ebf0, r#""\U0002ebf0""#),
            (0x2_ee5d, r#""\U0002ee5d""#),
            (0x3_1350, "\"\u{31350}\""),
            (0x3_23af, "\"\u{323af}\""),
            (0x3_23b0, r#""\U000323b0""#),
            (0x3_347b, r#""\U0003347b""#),
            // Plane 14 variation selectors: the last printable range there.
            (0xe_0100, "\"\u{e0100}\""),
            (0xe_01ef, "\"\u{e01ef}\""),
            (0xe_01f0, r#""\U000e01f0""#),
        ] {
            let c = char::from_u32(scalar).expect("test vectors are scalar values");
            let mut encoded = [0u8; 4];
            assert_eq!(
                go_quote(c.encode_utf8(&mut encoded).as_bytes()),
                want,
                "go_quote(U+{scalar:04X})"
            );
        }
    }

    #[test]
    fn go_is_print_matches_go_1_26_over_every_scalar() {
        // Exhaustive differential against strconv.IsPrint, reduced to two
        // constants so it needs no Go toolchain in the gate (the test
        // container has none). Reproduce both with:
        //
        //   for r := rune(0); r <= 0x10FFFF; r++ {
        //           if r >= 0xD800 && r <= 0xDFFF { continue }
        //           var b byte; if strconv.IsPrint(r) { b = 1; count++ }
        //           h ^= uint64(b); h *= 0x100000001b3   // from 0xcbf29ce484222325
        //   }
        //
        // Captured under go1.26.4 (Unicode 15.0.0). This is what makes a
        // single mistyped digit in the 1,177 transcribed table entries a gate
        // failure rather than one wrong error message years from now; it is
        // also what will fail loudly, rather than drift, if vendor/frankenphp
        // is ever bumped to a Go with newer unicode tables.
        let mut hash = Fnv::new();
        let mut printable = 0usize;
        let mut scalars = 0usize;
        for c in all_scalars() {
            let is_print = go_is_print(c);
            scalars += 1;
            printable += usize::from(is_print);
            hash.write(&[u8::from(is_print)]);
        }

        assert_eq!(scalars, 1_112_064, "scalar values visited");
        assert_eq!(printable, 148_998, "scalars strconv.IsPrint accepts");
        assert_eq!(
            hash.0, 0x23cf_2c1e_d413_9389,
            "FNV-1a of the IsPrint bit stream"
        );
    }

    #[test]
    fn go_quote_matches_go_1_26_over_a_generated_corpus() {
        // The same trick one level up: `%q` over a corpus both languages
        // generate identically -- every single byte, every scalar alone and
        // embedded between an ASCII letter and a lone 0x80 continuation byte
        // (so the valid/invalid transition is exercised at both ends), and
        // 200k xorshift64 byte soups of length 0..15 that are mostly not valid
        // UTF-8. Go side:
        //
        //   h.write([]byte(fmt.Sprintf("%q", s))); h.sep()
        //
        // over the same sequence, under go1.26.4.
        let mut hash = Fnv::new();
        let mut cases = 0usize;
        {
            let mut emit = |bytes: &[u8]| {
                hash.write(go_quote(bytes).as_bytes());
                hash.sep();
                cases += 1;
            };

            for b in 0..=u8::MAX {
                emit(&[b]);
            }

            let mut encoded = [0u8; 4];
            for c in all_scalars() {
                let bytes = c.encode_utf8(&mut encoded).as_bytes();
                emit(bytes);

                let mut embedded = Vec::with_capacity(bytes.len() + 2);
                embedded.push(b'x');
                embedded.extend_from_slice(bytes);
                embedded.push(0x80);
                emit(&embedded);
            }

            let mut rng = XorShift64(0x2545_f491_4f6c_dd1d);
            for _ in 0..200_000 {
                let len = (rng.next() % 16) as usize;
                let soup: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                emit(&soup);
            }
        }

        assert_eq!(cases, 2_424_384, "corpus size");
        assert_eq!(
            hash.0, 0xe6d9_c276_51d3_077e,
            "FNV-1a of every %q output in the corpus"
        );
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

        // The reviewer's repro for the printability oracle, at the level a
        // client actually reaches: F0 AE AF B0 is U+2EBF0, valid UTF-8 and
        // unassigned in the Unicode revision go1.26 generates its tables from,
        // so upstream escapes it rather than echoing the ideograph back.
        let unassigned = Request::new("POST", b"/".to_vec())
            .with_header("Content-Length", vec![0xf0, 0xae, 0xaf, 0xb0]);
        assert_eq!(
            validate_request(&unassigned).unwrap_err().message,
            r#"invalid Content-Length header: "\U0002ebf0""#
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
    fn request_uri_takes_raw_target_even_when_it_is_not_the_wire_bytes() {
        // `URL.RequestURI()` is not a slice of the request line. When the
        // wire path fails `EscapedPath()`'s round-trip check it is re-encoded
        // from the *decoded* path, so upstream turns the wire target
        // `/caf\xc3\xa9` into `/caf%C3%A9` and the wire target `/%2f"` into
        // `//%22` -- verified against go1.26.4's http.ReadRequest. Producing
        // that value belongs to whoever fills `raw_target`; this module's
        // contract is only that it copies whatever it is handed, byte for
        // byte, and never second-guesses it from `path` + `query`. Both cases
        // below have a `path` that a naive reconstruction would use instead,
        // and it differs from the answer in each direction.
        let re_escaped = Request::new("GET", "/café".as_bytes().to_vec())
            .with_raw_target(b"/caf%C3%A9".to_vec());
        assert_eq!(test_context(Some(re_escaped)).request_uri, b"/caf%C3%A9");

        let escape_decoded =
            Request::new("GET", b"//\"".to_vec()).with_raw_target(b"//%22".to_vec());
        assert_eq!(test_context(Some(escape_decoded)).request_uri, b"//%22");
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
    fn split_path_keeps_absent_and_explicitly_empty_distinguishable() {
        // Go's []string is nullable and `splitCgiPath` branches on precisely
        // that: `if splitPath == nil { splitPath = []string{".php"} }`
        // (cgi.go:195-197) defaults only nil, while a non-nil empty slice
        // reaches `splitPos`, which returns 0 (cgi.go:239-241) -- empty
        // DOCUMENT_URI, whole path as PATH_INFO. `WithRequestSplitPath([])`
        // reaches that state on purpose (requestoptions_test.go:39).
        //
        // Storing a `Vec<String>` would map both onto `vec![]`, and since
        // this struct is all the CGI layer gets, one of the two
        // configurations would then be unrecoverably wrong. So the
        // constructor must round-trip all three states unchanged, and must
        // not "helpfully" substitute the [".php"] default for `None` -- that
        // would re-erase the distinction one field later.
        let unconfigured = RequestContext::new(
            String::new(),
            None,
            Some(Request::new("GET", b"/index.php/foo".to_vec())),
            CompletionSignal::none(),
        );
        assert_eq!(
            unconfigured.split_path, None,
            "None is Go's nil: unconfigured, and the CGI layer -- not this \
             constructor -- supplies [\".php\"]"
        );

        let explicitly_empty = RequestContext::new(
            String::new(),
            Some(Vec::new()),
            Some(Request::new("GET", b"/index.php/foo".to_vec())),
            CompletionSignal::none(),
        );
        assert_eq!(
            explicitly_empty.split_path,
            Some(Vec::new()),
            "an explicit empty list is a different configuration from an \
             absent one, and must not collapse into it"
        );
        assert_ne!(
            unconfigured.split_path, explicitly_empty.split_path,
            "the two states drive different CGI splitting behaviour for the \
             same path, so they must remain distinguishable"
        );

        let configured = RequestContext::new(
            String::new(),
            Some(vec![".php".to_owned(), ".phtml".to_owned()]),
            None,
            CompletionSignal::none(),
        );
        assert_eq!(
            configured.split_path,
            Some(vec![".php".to_owned(), ".phtml".to_owned()]),
            "a configured list is stored verbatim -- normalising it is \
             WithRequestSplitPath's job, not this constructor's"
        );
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
            None,
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
            None,
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
