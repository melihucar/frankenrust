//! Port of the request-context half of `vendor/frankenphp/context.go`:
//! `frankenPHPContext` (`context.go:16-54`) becomes [`RequestContext`],
//! `fc.validate()` (`context.go:150-168`) becomes [`validate_request`], and
//! `fc.closeContext()` (`context.go:135-147`) becomes
//! [`RequestContext::close_context`].
//!
//! This module also owns the per-thread context slot table ([`ContextSlots`],
//! [`CONTEXT_SLOTS`]) -- the Rust analogue of `phpThread.frankenPHPContext()`
//! guarded by `phpThread.contextMu` (`vendor/frankenphp/threadregular.go:129-133`).
//! Issue #10 owns the thread registry itself but explicitly puts "request
//! plumbing" out of its scope, so #11 (this module) provides the slot table
//! `callbacks/servervars.rs` (and #12/#13/#14 after it) read and write,
//! keyed by `thread_index` alone.
//!
//! Skipped relative to upstream (see issue #11's spec): mercure,
//! `originalRequest` (`WithOriginalRequest`), the worker fields (#14), and
//! `handlerParameters`/`handlerReturn`.

use std::io::Read;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};

/// `request.TLS == nil` vs. non-nil (`cgi.go:54-70`) collapsed into an enum:
/// TLS itself is out of scope for this port (`docs/PORTING-NOTES.md`), so
/// nothing here ever negotiates a handshake or reports a real cipher/
/// protocol -- but the *value* of $_SERVER['HTTPS'] / REQUEST_SCHEME and the
/// SERVER_PORT 80/443 fallback are just a function of "was this connection
/// terminated as https", which whatever terminates TLS in front of us (a
/// future issue, or a reverse proxy) can set here without our needing to
/// implement TLS itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    #[default]
    Http,
    Https,
}

/// The request body, as `go_read_post` (#12's `callbacks/input.rs`) consumes
/// it.
///
/// Upstream's is `fc.request.Body`, an `io.ReadCloser` that `go_read_post`
/// reads incrementally on the PHP thread: PHP hands it a `count_bytes`-sized
/// C buffer and it loops `fc.request.Body.Read(p[readBytes:])` until the
/// buffer is full or a read errors (`frankenphp.go:683-694`).
/// `Box<dyn Read + Send>` is the direct analogue -- whatever accepts the
/// request decides what is behind it (an in-memory buffer, a socket, a
/// channel bridging the async side), and the PHP thread only ever calls
/// `read`.
///
/// `Send` but not `Sync`, matching how the context is used: it is built on
/// one thread, installed into the slot of the PHP thread that will run the
/// script, and read from that thread alone under the slot's `Mutex`. That is
/// the same single-consumer discipline `net/http` documents for
/// `Request.Body`, and it is why neither this type nor [`Request`] is
/// `Clone`: an `io.ReadCloser` has no meaningful copy either, and silently
/// handing out a clone with an empty body would lose a POST payload without
/// a word.
///
/// The read *loop*, the idle deadline (`WithRequestBodyTimeout`) and the
/// `isDone`/`responseWriter` guards around it (`frankenphp.go:666-681`) are
/// #12's to port; this is only the handle they read through.
#[derive(Default)]
pub struct RequestBody {
    reader: Option<Box<dyn Read + Send>>,
}

impl RequestBody {
    /// No body at all -- Go's `http.NoBody`. Every read reports EOF.
    pub fn empty() -> Self {
        Self { reader: None }
    }

    /// A body already buffered in memory. Bytes, not `String`: a POST body is
    /// arbitrary octets.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::from_reader(std::io::Cursor::new(bytes.into()))
    }

    /// A body streamed from `reader`, read on demand by the PHP thread.
    pub fn from_reader(reader: impl Read + Send + 'static) -> Self {
        Self {
            reader: Some(Box::new(reader)),
        }
    }

    /// Whether this request came with no body at all, as opposed to one that
    /// has merely been read to EOF. `go_read_post` does not need the
    /// distinction (both read 0), but `go_read_post`'s caller-side logging
    /// and `CONTENT_LENGTH` cross-checks may.
    pub fn is_none(&self) -> bool {
        self.reader.is_none()
    }
}

impl Read for RequestBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.reader {
            Some(reader) => reader.read(buf),
            None => Ok(0),
        }
    }
}

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A body is a stream: reading it to print it would consume it.
        f.debug_struct("RequestBody")
            .field("present", &self.reader.is_some())
            .finish()
    }
}

/// A multi-valued, case-insensitively-keyed header map, canonicalising names
/// the way Go's `net/http` does (`textproto.CanonicalMIMEHeaderKey`) on both
/// insert and lookup -- upstream's `commonHeaders` map
/// (`vendor/frankenphp/internal/phpheaders/phpheaders.go:15-118`) can be an
/// exact-match lookup only because Go's HTTP layer already canonicalised the
/// key before the handler saw it; we have no such layer upstream of us, so
/// we canonicalise ourselves rather than assume our caller did.
///
/// Values are raw bytes, not `String`: PHP strings are arbitrary bytes, and
/// header values must not be assumed UTF-8 (issue #11's hazards section).
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

    /// All values for `name`, joined with `", "` (`cgi.go:153`, `:161`) --
    /// byte-level concatenation, not UTF-8-validating. `None` if the header
    /// was never inserted; `Some(vec![])` if it was inserted with an empty
    /// value.
    ///
    /// This is the accessor for the `HTTP_*` mangling **only**. Everywhere
    /// upstream writes `request.Header.Get(...)` the right accessor is
    /// [`Headers::get_first`] -- see its doc comment.
    pub fn get_joined(&self, name: &str) -> Option<Vec<u8>> {
        let canon = canonical_header_name(name);
        self.entries
            .iter()
            .find(|(n, _)| *n == canon)
            .map(|(_, values)| join_bytes(values, b", "))
    }

    /// The **first** value for `name` -- Go's `Header.Get`, which is
    /// `textproto.MIMEHeader.Get` returning `v[0]` and never a join.
    ///
    /// This is what upstream uses at every one of its `Header.Get` call
    /// sites: `Content-Length` for `$_SERVER` (`cgi.go:93`) and for
    /// `validate()` (`context.go:157`), `Content-Type` (`cgi.go:306`) and
    /// `Authorization` (`cgi.go:316`). Only `addHeadersToServer`'s `HTTP_*`
    /// mangling joins duplicates. Reaching for [`Headers::get_joined`] here
    /// instead is a parser differential on fully client-controlled input:
    /// two `Content-Type` headers would become one value matching no
    /// registered POST reader, and two `Authorization` headers would reach
    /// `php_handle_auth_data` as un-decodable garbage.
    ///
    /// `None` if the header was never inserted; `Some(&[])` if it was
    /// inserted with an empty value (Go's `Get` cannot distinguish the two,
    /// so callers that need Go's exact behaviour treat both as absent).
    pub fn get_first(&self, name: &str) -> Option<&[u8]> {
        let canon = canonical_header_name(name);
        self.entries
            .iter()
            .find(|(n, _)| *n == canon)
            .and_then(|(_, values)| values.first())
            .map(Vec::as_slice)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[Vec<u8>])> {
        self.entries
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }

    /// Number of distinct header *names* (not values) -- used only as a
    /// `zend_hash_extend` sizing hint (`cgi.go:107`), same as upstream's
    /// `len(request.Header)`.
    pub fn name_count(&self) -> usize {
        self.entries.len()
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

/// Port of `textproto.CanonicalMIMEHeaderKey`.
///
/// Go bails out and returns the name **unchanged** as soon as it meets a byte
/// that is not a valid header field (token) byte, and otherwise only ever
/// flips ASCII letters -- so the canonical form of a well-formed name is pure
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

fn join_bytes(values: &[Vec<u8>], sep: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(value);
    }
    out
}

/// The inbound request. Mirrors the fields upstream's
/// `frankenPHPContext.request *http.Request` computation actually depends on
/// (`context.go:23`) -- not a general-purpose HTTP request type.
/// `frankenrust-core` has no hyper/http dependency (`docs/ARCHITECTURE.md`'s
/// crate-boundary section: that lives in `frankenrust-server`), so whatever
/// hands us a `Request` is responsible for filling it from the real
/// transport.
/// Not `Clone`: it owns the request body, which is a stream. See
/// [`RequestBody`].
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The **decoded** request path (Go's `request.URL.Path`). This is what
    /// `splitCgiPath` works on (`cgi.go:191`), and only that -- `REQUEST_URI`
    /// is built from [`Request::escaped_path`] instead, because upstream
    /// takes it from `r.URL.RequestURI()` (`context.go:111`), which is
    /// *escaped*. One field cannot serve both: `/index.php%2Fextra` must
    /// split as the decoded `/index.php/extra` while `$_SERVER['REQUEST_URI']`
    /// shows the escaped form.
    pub path: String,
    /// Go's `request.URL.RawPath`: the escaped path exactly as it arrived on
    /// the request line, left **empty** when `path` is already its own
    /// escaping (which is the overwhelmingly common case, and the condition
    /// under which `net/url` itself leaves `RawPath` empty).
    ///
    /// Whatever fills a `Request` from the wire owes the same invariant
    /// `net/url` maintains when it sets `RawPath`: set this only when it is a
    /// valid escaping of `path`. See [`Request::escaped_path`].
    pub raw_path: String,
    /// The raw, undecoded query string (Go's `request.URL.RawQuery`).
    pub raw_query: String,
    /// Go's `request.URL.ForceQuery`: set when the request target had a
    /// bare trailing `?` with nothing after it (e.g. `GET /index.php?
    /// HTTP/1.1`), which `net/url` parses as `RawQuery == ""` with
    /// `ForceQuery == true` rather than as "no query at all". Distinct from
    /// an absent query, and load-bearing for [`Request::request_uri`]: Go's
    /// `URL.RequestURI()` appends `"?"` when `ForceQuery || RawQuery != ""`,
    /// so a request line ending in a bare `?` must round-trip back out with
    /// one.
    pub force_query: bool,
    pub headers: Headers,
    /// Go's `request.RemoteAddr` -- may be malformed; `split_remote_addr`
    /// (in `cgi.rs`) must not panic on it.
    pub remote_addr: String,
    /// Go's `request.Host` -- the `Host` header (or HTTP/2 authority)
    /// verbatim, *not* derived from `headers`: net/http splits it out of the
    /// header map into its own field, and so do we.
    pub host: String,
    /// Go's `request.Proto`, e.g. `"HTTP/1.1"`.
    pub proto: String,
    pub proto_major: u16,
    pub proto_minor: u16,
    /// Go's `request.ContentLength`: the parsed framing length, `-1` if
    /// unknown/chunked. Distinct from the raw `Content-Length` *header*
    /// text, which lives in `headers` and is what `validate()` and
    /// `$_SERVER['CONTENT_LENGTH']` read.
    pub content_length: i64,
    pub scheme: Scheme,
    /// Analogue of Go's `request.Context()` cancellation
    /// (`docs/PORTING-NOTES.md`'s construct-mapping table: `context.Context`
    /// cancel -> `Arc<AtomicBool>` + a `Notify` on the async side). Whoever
    /// bridges the async side to this request flips this to `true` on
    /// client disconnect; `client_has_closed()` reads it.
    pub cancelled: Arc<AtomicBool>,
    pub body: RequestBody,
}

impl Request {
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        raw_query: impl Into<String>,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            raw_path: String::new(),
            raw_query: raw_query.into(),
            force_query: false,
            headers: Headers::default(),
            remote_addr: String::new(),
            host: String::new(),
            proto: "HTTP/1.1".to_string(),
            proto_major: 1,
            proto_minor: 1,
            content_length: -1,
            scheme: Scheme::Http,
            cancelled: Arc::new(AtomicBool::new(false)),
            body: RequestBody::empty(),
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<Vec<u8>>) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Attaches the request body #12's `go_read_post` will read through.
    /// Defaults to [`RequestBody::empty`], i.e. a bodyless request.
    pub fn with_body(mut self, body: RequestBody) -> Self {
        self.body = body;
        self
    }

    pub fn with_raw_path(mut self, raw_path: impl Into<String>) -> Self {
        self.raw_path = raw_path.into();
        self
    }

    /// See [`Request::force_query`]. Whatever fills a `Request` from the
    /// wire is responsible for setting this when the request line's target
    /// had a bare trailing `?` and an otherwise-empty query.
    pub fn with_force_query(mut self, force_query: bool) -> Self {
        self.force_query = force_query;
        self
    }

    /// Port of `URL.EscapedPath()` (Go's `net/url`), which is what
    /// `URL.RequestURI()` -- and so upstream's `fc.requestURI`
    /// (`context.go:111`) -- is built from.
    ///
    /// Deviation, deliberate: Go re-checks `validEncoded(RawPath) &&
    /// unescape(RawPath) == Path` before trusting `RawPath`, and falls back
    /// to escaping `Path` when that fails. It needs that check because
    /// `RawPath` is a public field a caller can scribble on; `net/url` itself
    /// only ever assigns `RawPath` when it *is* a valid escaping of `Path`.
    /// [`Request::raw_path`] documents the same obligation for our transport,
    /// so we trust it rather than re-implementing `net/url`'s unescaper to
    /// re-derive a fact its producer already knows.
    pub fn escaped_path(&self) -> String {
        if !self.raw_path.is_empty() {
            return self.raw_path.clone();
        }
        if self.path == "*" {
            // Go: don't escape (golang/go#11202) -- `OPTIONS *`.
            return "*".to_string();
        }
        escape_path(&self.path)
    }

    /// Port of `URL.RequestURI()` (Go's `net/url`): the escaped path, with
    /// `"?" + RawQuery` appended whenever `ForceQuery || RawQuery != ""`, and
    /// `"/"` standing in for an empty path. `Opaque` cannot occur on a
    /// server-received request (it is only ever set for non-hierarchical
    /// URIs like `mailto:`, which `net/http`'s request-line parsing never
    /// produces), so that part of upstream is not ported -- but `ForceQuery`
    /// can: a request line ending in a bare `?` (`GET /index.php?
    /// HTTP/1.1`) parses with `RawQuery == ""` and `ForceQuery == true`, and
    /// must round-trip back out with the `?`. See [`Request::force_query`].
    pub fn request_uri(&self) -> String {
        let mut uri = self.escaped_path();
        if uri.is_empty() {
            uri.push('/');
        }
        if self.force_query || !self.raw_query.is_empty() {
            uri.push('?');
            uri.push_str(&self.raw_query);
        }
        uri
    }
}

/// Port of Go's `shouldEscape(c, encodePath)` (`net/url`). The comment
/// upstream of the reserved-character list is Go's own: RFC 3986 allows
/// `: @ & = + $` in a path and saves `/ ; ,` for assigning meaning to
/// individual segments, but `net/url` only manipulates the path as a whole,
/// so it permits those three as well -- leaving only `?` to escape out of
/// that set.
fn should_escape_path_byte(c: u8) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    match c {
        // RFC 3986 §2.3 unreserved marks.
        b'-' | b'_' | b'.' | b'~' => false,
        // RFC 3986 §2.2 reserved, as filtered for `encodePath`.
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'@' => false,
        // Everything else, `?` included, must be escaped.
        _ => true,
    }
}

/// Port of Go's `escape(s, encodePath)` (`net/url`): `%XX` with upper-case
/// hex, and (unlike `encodeQueryComponent`) space escaped as `%20`, not `+`.
fn escape_path(path: &str) -> String {
    if !path.bytes().any(should_escape_path_byte) {
        return path.to_string();
    }

    const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if should_escape_path_byte(byte) {
            out.push('%');
            out.push(UPPER_HEX[(byte >> 4) as usize] as char);
            out.push(UPPER_HEX[(byte & 0x0f) as usize] as char);
        } else {
            // Every unescaped byte is ASCII (`should_escape_path_byte`
            // returns true for anything >= 0x80), so this cast is lossless.
            out.push(byte as char);
        }
    }
    out
}

/// The decision half of `fc.validate()` / `fc.reject()`
/// (`context.go:150-207`). Upstream's `reject()` writes this straight to an
/// `http.ResponseWriter`; this issue does not own a response writer (#13
/// does), so `RejectedRequest` only carries the verdict -- status and
/// message -- for whoever does own one to render.
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

/// Port of `fc.validate()` (`context.go:150-168`) -- the decision only, not
/// `reject()`'s response-writing side effect (see this module's doc comment
/// and issue #11's spec).
pub fn validate_request(request: &Request) -> Result<(), RejectedRequest> {
    if request.path.as_bytes().contains(&0) {
        return Err(RejectedRequest {
            status: 400,
            message: "invalid request path".to_string(),
        });
    }

    // `Header.Get` (context.go:157), i.e. the first value -- not a join of
    // all of them: two `Content-Length: 5` headers must validate exactly as
    // one does upstream, rather than be rejected as the non-numeric "5, 5".
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
                        message: format!(
                            "invalid Content-Length header: {:?}",
                            String::from_utf8_lossy(content_length)
                        ),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Allocation-stable arena for strings `SG(request_info)` borrows for the
/// request's whole lifetime (`docs/ARCHITECTURE.md`'s ownership scheme 2;
/// `frankenphp_free_request_context`, `frankenphp.c:361-373`, only NULLs the
/// five fields it pinned rather than freeing them -- ownership stays here).
///
/// Buffers are `Arc<[u8]>` rather than `Box<[u8]>`: both give the address
/// stability this arena's contract requires (a `Vec` reallocation moves the
/// handle, never the heap bytes it points at), but `Arc` additionally lets
/// this module's own tests prove the release-on-drop contract with
/// `Arc::downgrade`/`Weak::upgrade` instead of relying on unobservable
/// deallocation -- see `tests::drop_releases_arena_but_is_done_does_not`.
/// The extra atomic refcount is one bump per `alloc` call (a handful per
/// request), not a hot loop.
#[derive(Default)]
pub struct RequestArena {
    buffers: Vec<Arc<[u8]>>,
}

impl RequestArena {
    /// Copies `bytes` into a new heap allocation owned by this arena,
    /// appends a trailing NUL (PHP's SAPI reads several of these fields as
    /// C strings), and returns a pointer to it.
    ///
    /// Interior NUL bytes in `bytes` are preserved as-is rather than
    /// rejected: this follows the same rule upstream's `pinCString` does
    /// (`docs/PORTING-NOTES.md`'s construct-mapping table) -- a Go/Rust
    /// string may contain an embedded NUL, and a C reader using `strlen`
    /// simply truncates at the first one rather than erroring, which is the
    /// same outcome `CString::new` erroring here would defeat the point of.
    ///
    /// The returned pointer is valid for as long as this `RequestArena` (and
    /// therefore the `RequestContext` that owns it) is alive: each call
    /// pushes a new, independent `Arc<[u8]>` heap allocation into
    /// `self.buffers`, and growing that `Vec` moves the `Arc` handles
    /// (pointer + refcount pointer) around, never the bytes they point at --
    /// so a pointer returned by an earlier `alloc` call stays valid across
    /// later ones in the same `go_update_request_info` invocation, and for
    /// the rest of the request after that. Callers must not dereference the
    /// pointer once this arena has dropped.
    pub fn alloc(&mut self, bytes: &[u8]) -> *mut c_char {
        let mut buf = Vec::with_capacity(bytes.len() + 1);
        buf.extend_from_slice(bytes);
        buf.push(0);
        let arc: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
        let ptr = arc.as_ptr() as *mut c_char;
        self.buffers.push(arc);
        ptr
    }
}

/// Port of `frankenPHPContext` (`context.go:16-54`). See this module's doc
/// comment for what is intentionally not ported.
pub struct RequestContext {
    pub document_root: String,
    pub split_path: Vec<String>,
    pub request: Option<Request>,

    pub doc_uri: String,
    pub path_info: String,
    pub script_name: String,
    pub script_filename: String,
    pub request_uri: String,

    /// Whether the request is already closed by us (`context.go:37`).
    pub is_done: bool,
    /// The client's connection state as of the moment `is_done` was set
    /// (`context.go:38-45`) -- see `close_context`.
    pub client_had_closed: bool,

    /// Signals the async side that this request's script has finished
    /// writing a response body (`context.go:52`'s `done chan any`, closed by
    /// `closeContext()`). What is on the other end and how it becomes an
    /// HTTP response is #13/#14's async<->pthread bridge; this module only
    /// owns the slot and the `close_context` call that fires it.
    completion_signal: mpsc::Sender<()>,

    pub arena: RequestArena,

    /// Backing store for the `frankenrust_server_vars_batch` that
    /// `frankenrust-sys/shim.c`'s `go_register_server_variables` reads --
    /// installed by [`RequestContext::install_server_vars`], replaced on the
    /// next request that reuses this context, and otherwise reclaimed with
    /// the context itself.
    ///
    /// It lives here for the same reason [`RequestArena`] does, and it is the
    /// same reason: the C side may `zend_bailout()` partway through reading
    /// it, and a `longjmp` runs no Rust destructor. Anything owned by a Rust
    /// frame at that moment is leaked; anything owned by the context is not,
    /// because the context's reclaim point is the slot being cleared or
    /// replaced, which the `longjmp` cannot skip past. See
    /// [`crate::cgi::ServerVarsBatch`].
    server_vars: Option<crate::cgi::ServerVarsBatch>,
}

impl RequestContext {
    /// Runs `splitCgiPath` (`cgi.go:191-226`) once, at construction --
    /// exactly the point upstream's `NewRequestWithContext` calls it
    /// (`context.go:109`), not per request.
    ///
    /// Fails, like upstream's `NewRequestWithContext` chain does, when the
    /// configured split path is not valid: `split_pos`'s one-sided ASCII fold
    /// is only correct against entries that are themselves ASCII and
    /// lower-case, and it is `WithRequestSplitPath` (`requestoptions.go:86`)
    /// that establishes that -- see [`crate::cgi::normalize_split_path`].
    pub fn new(
        document_root: String,
        split_path: Option<Vec<String>>,
        request: Option<Request>,
        completion_signal: mpsc::Sender<()>,
    ) -> Result<Self, crate::cgi::InvalidSplitPath> {
        let split_path = match split_path {
            // `splitCgiPath`'s own default (cgi.go:195-197), already
            // normalised, so it cannot fail.
            None => vec![".php".to_string()],
            Some(configured) => crate::cgi::normalize_split_path(configured)?,
        };

        let (doc_uri, path_info, script_name, script_filename) = match &request {
            Some(r) => crate::cgi::split_cgi_path(&r.path, &split_path, &document_root),
            None => (String::new(), String::new(), String::new(), String::new()),
        };

        // `fc.requestURI = r.URL.RequestURI()` (context.go:111) -- the
        // *escaped* path plus the raw query, not the decoded `r.path` that
        // `split_cgi_path` above consumed. See `Request::request_uri`.
        let request_uri = match &request {
            Some(r) => r.request_uri(),
            None => String::new(),
        };

        Ok(Self {
            document_root,
            split_path,
            request,
            doc_uri,
            path_info,
            script_name,
            script_filename,
            request_uri,
            is_done: false,
            client_had_closed: false,
            completion_signal,
            arena: RequestArena::default(),
            server_vars: None,
        })
    }

    /// Takes ownership of `batch` and returns the C view of the *installed*
    /// copy, so that every pointer C receives targets memory this context
    /// owns rather than a Rust frame that is about to disappear.
    ///
    /// Replaces the previous request's batch, if any -- upstream re-pins per
    /// worker request too (`frankenphp.c:563` -> `:354`). That is safe at
    /// this point and only at this point: the previous batch can only still
    /// be reachable from C if a previous `go_register_server_variables` were
    /// still running on this thread, and a thread runs one request at a time.
    pub fn install_server_vars(
        &mut self,
        batch: crate::cgi::ServerVarsBatch,
    ) -> frankenrust_sys::frankenrust_server_vars_batch {
        self.server_vars.insert(batch).as_c_batch()
    }

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
    /// the client's connection state before signalling completion (signalling
    /// is what lets the async handler return, which is itself what would
    /// make a *later* read of `client_has_closed()` unreliable -- see
    /// upstream's comment at `context.go:38-45`), then marks `is_done`.
    ///
    /// Does **not** touch `arena`: the arena's reclaim point is the context
    /// slot being cleared or replaced (`ContextSlots::clear`/`set`), not
    /// response completion -- see this crate's `context` module doc and
    /// issue #11's "the trap" section. A worker script keeps running and
    /// writing after `fastcgi_finish_request()` calls this.
    pub fn close_context(&mut self) {
        if self.is_done {
            return;
        }
        self.client_had_closed = self.client_has_closed();
        let _ = self.completion_signal.send(());
        self.is_done = true;
    }
}

/// Per-thread request-context slots, keyed by `thread_index` -- the Rust
/// analogue of `phpThread.frankenPHPContext()` guarded by
/// `phpThread.contextMu` (`threadregular.go:129-133`). See this module's doc
/// comment for why #11 (not #10) owns this table.
///
/// `slots` is an `RwLock` guarding only *growth* (a rare event: the table
/// grows once per newly-seen `thread_index`); each thread's own `Mutex`
/// guards its slot for the hot path (set/get/clear on every request), so
/// unrelated PHP threads never contend with each other the way a single
/// global lock over the whole table would make them.
///
/// # The one rule for callers
///
/// **Never call into PHP from inside a [`ContextSlots::with_context`] /
/// [`ContextSlots::with_context_mut`] closure.** Copy out what you need,
/// return, and call C afterwards.
///
/// Any Zend routine can `zend_error_noreturn(E_ERROR, ...)` -- memory-limit
/// exhaustion being the ordinary case in worker mode, where the resident
/// worker script counts against the same limit -- and that ends in
/// `zend_bailout()`, a `longjmp` to a `zend_catch` that sits *above* our
/// frames on every path into these callbacks (`php_request_startup` wraps
/// `php_hash_environment()` in its own `zend_try`; worker mode's `$_SERVER`
/// re-import sits inside `frankenphp.c:565`'s). A `longjmp` runs no Rust
/// destructors, so a guard alive across such a call is leaked and the slot
/// is locked *forever* -- and the very next thing C does on that path
/// (`frankenphp.c:1591` -> `go_frankenphp_after_script_execution`) is clear
/// this slot, so the PHP thread would deadlock inside its own crash-recovery
/// path and the request would never be answered. A leaked read guard
/// additionally pins the table's reader count, so `ensure_len` blocks
/// forever the first time a new `thread_index` appears.
///
/// Upstream has the same shape for a different reason: `contextMu` guards
/// only the store (`threadregular.go:119-122`, `:131-134`) and the hot-path
/// reader `frankenPHPContext()` (`threadregular.go:77-79`) takes no lock at
/// all. `docs/PORTING-NOTES.md:126` states the rule in one line: "avoid
/// holding across an FFI call into PHP".
///
/// # And releasing the guard is not enough on its own
///
/// The leaked guard is only the *visible* half. Rust has no defined
/// behaviour for a `longjmp` crossing one of its frames at all -- not merely
/// for one holding a destructor -- so a callback that calls a bail-out-capable
/// PHP function from a Rust frame is unsound however carefully it drops
/// things first, and catching the bailout in a C `zend_try` does not fix it
/// either (the re-raise jumps back out across that same Rust frame).
///
/// The only shape that works is to keep Rust off the stack for the part that
/// can bail out: a C entry point that calls Rust to compute, takes the
/// result, and *then* calls PHP. `go_register_server_variables` is done that
/// way -- `crates/frankenrust-sys/shim.c` plus
/// [`crate::callbacks::servervars::frankenrust_collect_server_vars`] -- and
/// any later callback that touches PHP request memory (#12's output/input
/// callbacks, #13/#14's lifecycle) needs the same treatment or a
/// demonstration that what it calls cannot reach `zend_bailout()`. See issue
/// #75.
pub struct ContextSlots {
    slots: RwLock<Vec<Mutex<Option<RequestContext>>>>,
}

impl ContextSlots {
    pub const fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
        }
    }

    fn ensure_len(&self, thread_index: usize) {
        let needed = thread_index + 1;
        if self.slots.read().unwrap().len() >= needed {
            return;
        }
        let mut slots = self.slots.write().unwrap();
        while slots.len() < needed {
            slots.push(Mutex::new(None));
        }
    }

    /// Installs `ctx` as `thread_index`'s context, dropping (and so
    /// releasing the arena of) whatever context previously occupied the
    /// slot, if any.
    pub fn set(&self, thread_index: usize, ctx: RequestContext) {
        self.ensure_len(thread_index);
        let slots = self.slots.read().unwrap();
        *slots[thread_index].lock().unwrap() = Some(ctx);
    }

    /// Drops `thread_index`'s context, if any -- releasing its arena.
    pub fn clear(&self, thread_index: usize) {
        self.ensure_len(thread_index);
        let slots = self.slots.read().unwrap();
        *slots[thread_index].lock().unwrap() = None;
    }

    /// Runs `f` with `thread_index`'s context, holding that slot's lock for
    /// the duration. `f` must not call into PHP -- see this type's "one rule
    /// for callers".
    pub fn with_context<R>(
        &self,
        thread_index: usize,
        f: impl FnOnce(Option<&RequestContext>) -> R,
    ) -> R {
        self.ensure_len(thread_index);
        let slots = self.slots.read().unwrap();
        let guard = slots[thread_index].lock().unwrap();
        f(guard.as_ref())
    }

    /// Mutable counterpart of [`ContextSlots::with_context`], needed by
    /// anything that pushes into the context's arena. The same rule applies:
    /// `f` must not call into PHP.
    pub fn with_context_mut<R>(
        &self,
        thread_index: usize,
        f: impl FnOnce(Option<&mut RequestContext>) -> R,
    ) -> R {
        self.ensure_len(thread_index);
        let slots = self.slots.read().unwrap();
        let mut guard = slots[thread_index].lock().unwrap();
        f(guard.as_mut())
    }
}

impl Default for ContextSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// The one instance every callback in `callbacks::servervars` (and, later,
/// #12/#13/#14) reads and writes through.
pub static CONTEXT_SLOTS: ContextSlots = ContextSlots::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(request: Option<Request>) -> RequestContext {
        let (tx, _rx) = mpsc::channel();
        RequestContext::new("/var/www".to_string(), None, request, tx)
            .expect("the default split path is valid")
    }

    #[test]
    fn headers_join_multi_valued_with_comma_space() {
        let mut headers = Headers::default();
        headers.insert("X-Foo", b"a".to_vec());
        headers.insert("X-Foo", b"b".to_vec());
        assert_eq!(headers.get_joined("X-Foo").unwrap(), b"a, b");
    }

    #[test]
    fn headers_lookup_is_canonicalisation_insensitive() {
        let mut headers = Headers::default();
        headers.insert("accept-ENCODING", b"gzip".to_vec());
        assert_eq!(
            headers.get_joined("Accept-Encoding").unwrap(),
            b"gzip",
            "lookup must canonicalise regardless of how the name was inserted"
        );
    }

    #[test]
    fn headers_present_but_empty_is_some_empty() {
        let mut headers = Headers::default();
        headers.insert("Content-Length", Vec::new());
        assert_eq!(headers.get_joined("Content-Length"), Some(Vec::new()));
        assert_eq!(headers.get_joined("Missing"), None);
    }

    #[test]
    fn headers_get_first_returns_first_value_not_a_join() {
        // Regression test for the review finding that `Header.Get`'s call
        // sites (Content-Type, Authorization, Content-Length) were reading
        // the ", "-joined accessor. Go's MIMEHeader.Get returns v[0].
        let mut headers = Headers::default();
        headers.insert("Content-Type", b"text/plain".to_vec());
        headers.insert("Content-Type", b"application/json".to_vec());

        assert_eq!(headers.get_first("Content-Type").unwrap(), b"text/plain");
        assert_eq!(
            headers.get_joined("Content-Type").unwrap(),
            b"text/plain, application/json",
            "the joining accessor stays available -- addHeadersToServer needs it"
        );
        assert_eq!(headers.get_first("Missing"), None);
    }

    #[test]
    fn canonical_header_name_leaves_non_token_names_unchanged() {
        // textproto.CanonicalMIMEHeaderKey bails out on the first byte that
        // is not a valid header field byte and returns the name verbatim.
        // Without that bail-out a byte >= 0x80 would be re-encoded as two
        // UTF-8 bytes by `byte as char` and silently corrupted.
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
    fn request_uri_is_the_escaped_path_not_the_decoded_one() {
        // fc.requestURI = r.URL.RequestURI() (context.go:111), which is built
        // from EscapedPath(); splitCgiPath meanwhile works on the decoded
        // URL.Path. One field cannot be both, so `raw_path` carries the
        // escaped form when it differs.
        let request =
            Request::new("GET", "/index.php/extra", "x=1").with_raw_path("/index.php%2Fextra");
        assert_eq!(request.request_uri(), "/index.php%2Fextra?x=1");

        let ctx = test_context(Some(request));
        assert_eq!(ctx.request_uri, "/index.php%2Fextra?x=1");
        assert_eq!(
            ctx.script_name, "/index.php",
            "path splitting still runs on the decoded path"
        );
        assert_eq!(ctx.path_info, "/extra");
    }

    #[test]
    fn request_uri_escapes_the_decoded_path_when_no_raw_path_is_set() {
        // Go's EscapedPath() falls back to escape(Path, encodePath) when
        // RawPath is empty: space -> %20 (not '+'), '?' escaped, ':' '@' '='
        // '&' '+' '$' ',' ';' '/' and the unreserved marks left alone.
        assert_eq!(
            Request::new("GET", "/index.php", "").request_uri(),
            "/index.php"
        );
        assert_eq!(Request::new("GET", "/a b", "").request_uri(), "/a%20b");
        assert_eq!(Request::new("GET", "/a?b", "").request_uri(), "/a%3Fb");
        assert_eq!(
            Request::new("GET", "/a~b-c_d.e", "").request_uri(),
            "/a~b-c_d.e"
        );
        assert_eq!(
            Request::new("GET", "/a:b@c=d&e+f$g,h;i", "").request_uri(),
            "/a:b@c=d&e+f$g,h;i"
        );
        assert_eq!(
            Request::new("GET", "/caf\u{e9}", "").request_uri(),
            "/caf%C3%A9",
            "non-ASCII is escaped byte-wise, as Go's escape() does"
        );
        assert_eq!(
            Request::new("GET", "", "").request_uri(),
            "/",
            "RequestURI() substitutes \"/\" for an empty path"
        );
        assert_eq!(
            Request::new("OPTIONS", "*", "").request_uri(),
            "*",
            "golang/go#11202: `OPTIONS *` is not escaped"
        );
    }

    #[test]
    fn request_uri_keeps_a_forced_empty_query() {
        // GET /index.php? HTTP/1.1 parses (net/url) as RawQuery == "" with
        // ForceQuery == true, not as "no query at all" -- URL.RequestURI()
        // appends "?" for either `ForceQuery || RawQuery != ""`, so the two
        // cases must not collapse to the same output.
        let forced = Request::new("GET", "/index.php", "").with_force_query(true);
        assert_eq!(forced.request_uri(), "/index.php?");

        let absent = Request::new("GET", "/index.php", "");
        assert_eq!(
            absent.request_uri(),
            "/index.php",
            "an absent query must not gain a trailing '?'"
        );

        let real_query = Request::new("GET", "/index.php", "x=1").with_force_query(true);
        assert_eq!(
            real_query.request_uri(),
            "/index.php?x=1",
            "force_query is redundant but harmless when a real query is present"
        );
    }

    #[test]
    fn new_normalises_an_uppercase_split_path_and_rejects_a_non_ascii_one() {
        // WithRequestSplitPath (requestoptions.go:86-113) lower-cases ASCII
        // entries and rejects non-ASCII ones; split_pos folds only the bytes
        // of the *path*, so an un-normalised entry silently never matches.
        let (tx, _rx) = mpsc::channel();
        let ctx = RequestContext::new(
            "/var/www".to_string(),
            Some(vec![".PHP".to_string()]),
            Some(Request::new("GET", "/index.php/foo", "")),
            tx,
        )
        .expect(".PHP is ASCII, so it normalises rather than failing");
        assert_eq!(ctx.script_name, "/index.php");
        assert_eq!(ctx.path_info, "/foo");

        let (tx, _rx) = mpsc::channel();
        let rejected = RequestContext::new(
            "/var/www".to_string(),
            Some(vec![".php".to_string(), ".Ⱥphp".to_string()]),
            Some(Request::new("GET", "/index.php", "")),
            tx,
        );
        assert_eq!(rejected.err(), Some(crate::cgi::InvalidSplitPath));
    }

    #[test]
    fn validate_rejects_nul_byte_in_path() {
        let request = Request::new("GET", "/foo\0bar", "");
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_rejects_non_numeric_content_length() {
        let request = Request::new("POST", "/", "").with_header("Content-Length", b"abc".to_vec());
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_rejects_negative_content_length() {
        let request = Request::new("POST", "/", "").with_header("Content-Length", b"-1".to_vec());
        let err = validate_request(&request).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_accepts_duplicate_content_length_headers() {
        // Go reads Header.Get("Content-Length") -> "5"; joining the two into
        // "5, 5" would fail to parse and turn a request upstream accepts into
        // a 400.
        let request = Request::new("POST", "/", "")
            .with_header("Content-Length", b"5".to_vec())
            .with_header("Content-Length", b"5".to_vec());
        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn validate_accepts_valid_content_length_and_no_header() {
        let plain = Request::new("GET", "/", "");
        assert!(validate_request(&plain).is_ok());

        let with_length =
            Request::new("POST", "/", "").with_header("Content-Length", b"42".to_vec());
        assert!(validate_request(&with_length).is_ok());
    }

    #[test]
    fn drop_releases_arena_but_is_done_does_not() {
        let mut ctx = test_context(Some(Request::new("GET", "/", "")));
        ctx.arena.alloc(b"hello");
        let weak = Arc::downgrade(&ctx.arena.buffers[0]);
        assert!(
            weak.upgrade().is_some(),
            "sanity: buffer alive right after alloc"
        );

        ctx.close_context();
        assert!(ctx.is_done);
        assert!(
            weak.upgrade().is_some(),
            "marking is_done must not release the arena -- a worker script keeps \
             writing after fastcgi_finish_request()"
        );

        drop(ctx);
        assert!(
            weak.upgrade().is_none(),
            "dropping the RequestContext must release its arena"
        );
    }

    /// The reviewed defect this pins down: `RequestBody` was a fieldless
    /// placeholder, so a body could not be attached to a request at all. #12
    /// owns `go_read_post` and is explicitly forbidden from editing this
    /// file, which left it with nothing to read -- every POST would have
    /// reached PHP with an empty `php://input` and no parsed `$_POST`.
    ///
    /// This is the exact shape `go_read_post` needs (`frankenphp.go:683-694`):
    /// reach the body from a thread index, read into a fixed-size buffer,
    /// repeat until it is full, and see EOF as a 0. The read *loop* is #12's;
    /// what this asserts is that the handle supports one.
    #[test]
    fn a_request_body_is_readable_incrementally_through_the_context_slot() {
        let slots = ContextSlots::new();
        slots.set(
            3,
            test_context(Some(
                Request::new("POST", "/index.php", "")
                    .with_body(RequestBody::from_bytes(b"hello world".to_vec())),
            )),
        );

        let read_chunk = |len: usize| -> Vec<u8> {
            slots.with_context_mut(3, |ctx| {
                let body = &mut ctx
                    .expect("context installed")
                    .request
                    .as_mut()
                    .unwrap()
                    .body;
                let mut buf = vec![0u8; len];
                let n = body.read(&mut buf).expect("in-memory body cannot fail");
                buf.truncate(n);
                buf
            })
        };

        assert_eq!(read_chunk(5), b"hello");
        assert_eq!(read_chunk(6), b" world");
        assert_eq!(
            read_chunk(4),
            b"",
            "a body read to the end reports EOF as a 0-length read, which is what \
             ends go_read_post's loop"
        );
    }

    #[test]
    fn a_bodyless_request_reads_eof_immediately() {
        // Go's http.NoBody. `Request::new` installs this, so a GET built by
        // any of this module's tests reads 0 rather than blocking or panicking.
        let mut request = Request::new("GET", "/index.php", "");
        assert!(request.body.is_none());

        let mut buf = [0u8; 8];
        assert_eq!(request.body.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn a_streamed_request_body_is_read_on_demand() {
        // Not every body is buffered: the transport may hand us a socket. The
        // handle must accept any `Read + Send`, since the context is built on
        // one thread and read on the PHP thread that owns its slot.
        struct OneByteAtATime(std::io::Cursor<Vec<u8>>);
        impl Read for OneByteAtATime {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if buf.is_empty() {
                    return Ok(0);
                }
                self.0.read(&mut buf[..1])
            }
        }

        let mut body =
            RequestBody::from_reader(OneByteAtATime(std::io::Cursor::new(b"php".to_vec())));
        assert!(!body.is_none());

        // A short read is exactly the case frankenphp.go:685-694 loops over.
        let mut buf = [0u8; 3];
        assert_eq!(body.read(&mut buf).unwrap(), 1);
        assert_eq!(&buf[..1], b"p");

        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut body, &mut rest).unwrap();
        assert_eq!(rest, b"hp");
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
        let mut first = test_context(None);
        first.arena.alloc(b"first");
        let weak = Arc::downgrade(&first.arena.buffers[0]);
        slots.set(2, first);

        slots.set(2, test_context(None));
        assert!(
            weak.upgrade().is_none(),
            "set() must drop (and so release the arena of) the previous context"
        );
    }
}
