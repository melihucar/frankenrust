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

/// Placeholder for the request-body bridge #12's `callbacks/input.rs`
/// (`go_read_post`) will read through. No callback this issue implements
/// touches it; it exists only because `RequestContext` needs a slot for it
/// (issue #11's field list: "the request itself (method, URI, query,
/// headers, body handle)").
#[derive(Debug, Clone, Default)]
pub struct RequestBody;

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
    /// value (matches Go's `Header.Get`, which cannot distinguish the two
    /// either).
    pub fn get_joined(&self, name: &str) -> Option<Vec<u8>> {
        let canon = canonical_header_name(name);
        self.entries
            .iter()
            .find(|(n, _)| *n == canon)
            .map(|(_, values)| join_bytes(values, b", "))
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

fn canonical_header_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper_next = true;
    for byte in name.bytes() {
        let cased = if upper_next {
            byte.to_ascii_uppercase()
        } else {
            byte.to_ascii_lowercase()
        };
        // Header field names are ASCII tokens; a byte outside a-z/A-Z is
        // passed through unchanged by to_ascii_*case, and casting it to
        // `char` reproduces it exactly (values 0x00-0x7F round-trip through
        // `char` losslessly; higher bytes should never occur in a
        // conformant header name, and if one does this stays non-panicking).
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
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The decoded request path (Go's `request.URL.Path`).
    pub path: String,
    /// The raw, undecoded query string (Go's `request.URL.RawQuery`).
    pub raw_query: String,
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
            raw_query: raw_query.into(),
            headers: Headers::default(),
            remote_addr: String::new(),
            host: String::new(),
            proto: "HTTP/1.1".to_string(),
            proto_major: 1,
            proto_minor: 1,
            content_length: -1,
            scheme: Scheme::Http,
            cancelled: Arc::new(AtomicBool::new(false)),
            body: RequestBody,
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<Vec<u8>>) -> Self {
        self.headers.insert(name, value);
        self
    }
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

    if let Some(content_length) = request.headers.get_joined("Content-Length") {
        if !content_length.is_empty() {
            let parsed = std::str::from_utf8(&content_length)
                .ok()
                .and_then(|s| s.parse::<i64>().ok());
            match parsed {
                Some(n) if n >= 0 => {}
                _ => {
                    return Err(RejectedRequest {
                        status: 400,
                        message: format!(
                            "invalid Content-Length header: {:?}",
                            String::from_utf8_lossy(&content_length)
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
}

impl RequestContext {
    /// Runs `splitCgiPath` (`cgi.go:191-226`) once, at construction --
    /// exactly the point upstream's `NewRequestWithContext` calls it
    /// (`context.go:109`), not per request.
    pub fn new(
        document_root: String,
        split_path: Option<Vec<String>>,
        request: Option<Request>,
        completion_signal: mpsc::Sender<()>,
    ) -> Self {
        let split_path = split_path.unwrap_or_else(|| vec![".php".to_string()]);

        let (doc_uri, path_info, script_name, script_filename) = match &request {
            Some(r) => crate::cgi::split_cgi_path(&r.path, &split_path, &document_root),
            None => (String::new(), String::new(), String::new(), String::new()),
        };

        // Go's `fc.requestURI = r.URL.RequestURI()` (context.go:111): the
        // encoded path plus, if present, "?" and the raw query. We do not
        // have `net/url`'s re-escaping machinery; `r.path` is taken as
        // already being the request-line path our caller received.
        let request_uri = match &request {
            Some(r) if r.raw_query.is_empty() => r.path.clone(),
            Some(r) => format!("{}?{}", r.path, r.raw_query),
            None => String::new(),
        };

        Self {
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
        }
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
