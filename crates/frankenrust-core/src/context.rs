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
//! # The one rule for [`ContextSlots`] callers
//!
//! **Never call into PHP from inside a [`ContextSlots::with_context`] /
//! [`ContextSlots::with_context_mut`] closure.** See that type's doc comment
//! for why: a Zend bailout `longjmp`s past Rust destructors, so a lock guard
//! (or anything else) alive on the stack at that moment is never released.

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

    /// The request-target exactly as it appeared on the request line -- Go's
    /// `Request.RequestURI` field, verbatim, before any parsing. Percent
    /// escapes and a bare trailing `?` (an empty query the client still
    /// asked for -- Go's `URL.ForceQuery`) survive here unmodified; neither
    /// can be recovered from [`Request::path`] once it has been decoded.
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
/// [`ContextSlots`]) -- so it must not block, must not call into PHP, and
/// must not panic. Waking a oneshot receiver satisfies all three.
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
/// Buffers are `Arc<[u8]>` rather than `Box<[u8]>` for the same address
/// stability either would give (a `Vec` growing moves the handle, never the
/// heap bytes behind it), plus one extra thing a `Box` cannot: this module's
/// own tests prove the release-on-drop half of the contract with
/// `Arc::downgrade` / `Weak::upgrade`, rather than relying on unobservable
/// deallocation. The extra atomic refcount bump is once per `alloc` call (a
/// handful per request), not a hot loop.
#[derive(Default)]
pub struct RequestArena {
    buffers: Vec<Arc<[u8]>>,
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
    /// SAFETY (for callers that go on to dereference the returned pointer):
    /// the pointer is valid for exactly as long as this `RequestArena` (and
    /// so the `RequestContext` that owns it) is alive. Each call pushes a
    /// new, independent `Arc<[u8]>` into `self.buffers`; that `Arc` owns one
    /// heap allocation holding its refcounts and byte data together, separate
    /// from the `Vec`'s own backing storage. Growing `self.buffers` (a `Vec`
    /// of 16-byte fat pointers -- address plus length) reallocates and moves
    /// *those* handles around, but never the allocation any one of them
    /// points to, and that allocation is not freed while the `Arc` in
    /// `self.buffers` keeps it alive. So a pointer returned by an earlier
    /// call stays valid across later ones, and for the rest of the request
    /// after that. It stops being valid the moment this arena drops -- see
    /// [`RequestContext::close_context`] for why that is not the same moment
    /// as the request being marked done.
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
/// lock over the whole table would make them.
///
/// # The one rule for callers
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
pub struct ContextSlots {
    slots: RwLock<Vec<Mutex<Option<RequestContext>>>>,
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

impl ContextSlots {
    pub const fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
        }
    }

    fn ensure_len(&self, thread_index: usize) {
        let needed = thread_index + 1;
        if recover_read(&self.slots).len() >= needed {
            return;
        }
        let mut slots = self
            .slots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while slots.len() < needed {
            slots.push(Mutex::new(None));
        }
    }

    /// Installs `ctx` as `thread_index`'s context, dropping (and so
    /// releasing the arena of) whatever context previously occupied the
    /// slot, if any.
    pub fn set(&self, thread_index: usize, ctx: RequestContext) {
        self.ensure_len(thread_index);
        let slots = recover_read(&self.slots);
        *recover_lock(&slots[thread_index]) = Some(ctx);
    }

    /// Drops `thread_index`'s context, if any -- releasing its arena.
    pub fn clear(&self, thread_index: usize) {
        self.ensure_len(thread_index);
        let slots = recover_read(&self.slots);
        *recover_lock(&slots[thread_index]) = None;
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
        let slots = recover_read(&self.slots);
        let guard = recover_lock(&slots[thread_index]);
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
        let slots = recover_read(&self.slots);
        let mut guard = recover_lock(&slots[thread_index]);
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

        // Push enough further entries to force the arena's own Vec<Arc<[u8]>>
        // spine to reallocate (likely many times over).
        for i in 0..10_000 {
            arena.alloc(format!("entry-{i}").as_bytes());
        }

        // SAFETY: `arena` is still alive and owns `first`'s backing buffer;
        // RequestArena::alloc documents that a returned pointer stays valid
        // across later `alloc` calls, because growing the spine moves the
        // `Arc` handles, never the heap bytes they point at.
        let bytes = unsafe { std::ffi::CStr::from_ptr(first) };
        assert_eq!(bytes.to_bytes(), b"hello");
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
        let mut ctx = test_context(Some(Request::new("GET", b"/".to_vec())));
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
    fn context_slots_concurrent_access_neither_deadlocks_nor_loses_a_slot() {
        let slots = Arc::new(ContextSlots::new());

        // Distinct indices, hammered concurrently: exercises the growth path
        // (ensure_len) racing across threads. If a slot were ever lost, the
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
