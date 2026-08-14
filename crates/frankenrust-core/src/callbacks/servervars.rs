//! `frankenrust_collect_server_vars`, `go_update_request_info` -- bulk
//! `$_SERVER` import and `sapi_request_info` population
//! (`vendor/frankenphp/frankenphp.c:1379`, `:355`). Real bodies for issue
//! #11, over the CGI/`$_SERVER` byte helpers in [`crate::cgi`] (#80, the
//! port of the pure half of `cgi.go`) and the per-thread context table in
//! [`crate::context`] (#79).
//!
//! # Why `go_register_server_variables` is not here
//!
//! It is the one `go_*` callback whose C-ABI entry point is written in C, in
//! `crates/frankenrust-sys/shim.c`, and this module supplies only the half of
//! it that touches no PHP API ([`frankenrust_collect_server_vars`]).
//!
//! Every function that callback calls -- `frankenphp_register_server_vars`,
//! `frankenphp_register_known_variable`, `frankenphp_register_variable_safe`
//! -- allocates through the Zend *request* allocator, which on `memory_limit`
//! exhaustion ends in `zend_bailout()`: a `longjmp` to a `zend_catch` above
//! the callback. Go tolerates that jump crossing a live cgo frame; Rust has no
//! defined behaviour for it crossing a Rust frame, and -- the trap two
//! earlier designs for this issue fell into -- catching the bailout in C and
//! re-raising it from Rust does not help, because the re-raise is itself a
//! `longjmp` out of the Rust callback frame. Dropping every payload first
//! removes the leak, not the undefined behaviour.
//!
//! So the split is structural rather than defensive: Rust computes and
//! *returns*, then C registers with no Rust frame anywhere between
//! `zend_bailout()` and `php_request_startup`'s `zend_catch`. No `zend_try` of
//! our own is involved and PHP's control flow is bit-for-bit upstream's. See
//! `shim.c`'s header comment.
//!
//! # The empty-value rule
//!
//! Every `char *`/length pair this module hands to C represents an absent or
//! empty value the same way: a NULL pointer with length 0 (see [`c_buf`]).
//! Upstream passes `toUnsafeChar(s)` for an empty Go string `s`, and Go's
//! `unsafe.StringData("")` is documented as *unspecified* -- it may or may not
//! return nil -- so there is no upstream behaviour to match here in the first
//! place. NULL is the deterministic choice, it is one of the two answers Go
//! may give, and it avoids ever handing C the non-NULL dangling pointer
//! `Vec::new().as_ptr()` produces. It is also exactly what
//! `frankenphp_register_trusted_var` (`frankenphp.c:1199-1218`) already
//! special-cases: `value == NULL` inserts `ZVAL_EMPTY_STRING` directly and
//! skips `sapi_module.input_filter`, and `frankenphp_register_variable_safe`
//! (`frankenphp.c:1350-1352`) does its own `if (val == NULL) val = "";` --
//! both sides of the header path handle it too.

use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::OnceLock;

use frankenrust_sys::{
    frankenphp_server_vars, frankenrust_header_var, frankenrust_server_vars_batch,
    sapi_request_info, zend_string,
};

use crate::cgi;
use crate::context::{RequestContext, CONTEXT_SLOTS};

// ---------------------------------------------------------------------------
// The empty-value rule, shared by every char*/len pair this module builds.
// ---------------------------------------------------------------------------

/// See this module's doc comment. `bytes.is_empty()` covers both "the value
/// was never present" and "the value was present and empty" -- upstream
/// cannot tell those apart either (`Header.Get` returns `""` for both), so
/// this module does not try to.
fn c_buf(bytes: &[u8]) -> (*mut c_char, usize) {
    if bytes.is_empty() {
        (std::ptr::null_mut(), 0)
    } else {
        (bytes.as_ptr() as *mut c_char, bytes.len())
    }
}

// ---------------------------------------------------------------------------
// Interned strings (frankenphp.c:1277-1301)
// ---------------------------------------------------------------------------

/// A `zend_string*` from `frankenphp_init_persistent_string`: permanently
/// allocated, `IS_STR_INTERNED`-flagged, never refcounted or freed
/// (`frankenphp.c:1278-1287`).
#[derive(Clone, Copy)]
struct InternedZendString(*mut zend_string);

// SAFETY: `IS_STR_INTERNED` means the per-request GC ignores this
// `zend_string` and it is never refcounted -- there is nothing mutable
// about it for two threads to race on, so sharing the raw pointer across
// threads (this type only ever lives inside the process-lifetime
// `INTERNED` OnceLock) is sound.
unsafe impl Send for InternedZendString {}
// SAFETY: see the `Send` impl above -- the same absence of interior
// mutability makes concurrent shared access sound too.
unsafe impl Sync for InternedZendString {}

/// The `zend_string*`s this module mints itself rather than reading
/// upstream's `frankenphp_strings` (`frankenphp.h:145`): that global is only
/// populated by the main-thread-only, `static`
/// `frankenphp_init_interned_strings()` (`frankenphp.c:1290-1301`), which is
/// #10's territory (`mainthread.rs`) and reads NULL in any unit test that
/// never boots a PHP main thread. It also buys nothing here --
/// `frankenphp_register_server_vars` consumes `request_scheme`/`ssl_protocol`/
/// `https` only as `ZVAL_STR(&zv, vars.<field>)` (`frankenphp.c:1255-1262`),
/// with no pointer-identity requirement on which interned string a given
/// value is.
///
/// All four of upstream's scheme-related strings are minted for symmetry with
/// `FRANKENPHP_INTERNED_STRINGS_LIST`'s `httpLowercase`/`httpsLowercase`/`on`/
/// `empty`, even though [`compute_server_vars`] only ever reaches two of them
/// today: `Request` has no TLS field (TLS is out of scope for this port, see
/// `docs/PORTING-NOTES.md`), so the scheme is always
/// [`cgi::Scheme::Http`] and `https_scheme`/`on` are dead code until a later
/// issue adds one.
struct InternedStrings {
    common_headers: HashMap<&'static str, InternedZendString>,
    http_scheme: InternedZendString,
    #[allow(
        dead_code,
        reason = "minted for symmetry -- see this struct's doc comment"
    )]
    https_scheme: InternedZendString,
    #[allow(
        dead_code,
        reason = "minted for symmetry -- see this struct's doc comment"
    )]
    on: InternedZendString,
    empty: InternedZendString,
}

static INTERNED: OnceLock<InternedStrings> = OnceLock::new();

/// Upstream builds its equivalent (`frankenphp_strings`) once, on the main
/// thread, after boot reaches Ready (`phpmainthread.go:121-125`) -- that
/// ordering lives in `mainthread.rs`, which issue #10 owns and whose spec
/// never mentions this. The in-lane answer for #11 is to build these lazily
/// instead, in a `OnceLock`, on first use. This deviates from upstream's
/// initialisation *ordering* but not its safety properties:
/// `frankenphp_init_persistent_string` is `zend_string_init(...,
/// persistent=1)` (a plain `malloc`) + `zend_string_hash_val` (pure
/// computation) + `GC_ADD_FLAGS(IS_STR_INTERNED)` (a flag write on freshly
/// allocated memory nothing else can see yet) -- none of it touches TSRM, so
/// it has no dependency on which thread, or how early, it runs on. Upstream's
/// own `frankenphp_init_interned_strings` is likewise self-guarding rather
/// than relying on being called exactly once (`frankenphp.c:1290-1293`:
/// `if (frankenphp_strings.remote_addr != NULL) { return; }`), which is the
/// same "safe to call from more than one place, idempotent in effect"
/// property `OnceLock` gives us directly.
fn interned_strings() -> &'static InternedStrings {
    INTERNED.get_or_init(|| InternedStrings {
        common_headers: cgi::COMMON_HEADERS
            .iter()
            .map(|&(name, variable)| (name, intern(variable)))
            .collect(),
        http_scheme: intern("http"),
        https_scheme: intern("https"),
        on: intern("on"),
        empty: intern(""),
    })
}

fn intern(s: &str) -> InternedZendString {
    // SAFETY: `frankenphp_init_persistent_string` (`frankenphp.c:1278-1287`)
    // is a plain persistent malloc + memcpy with no TSRM dependency (see the
    // doc comment on `interned_strings` above), so it is sound to call from
    // any thread at any time, including before `ts_resource()` has run on it.
    // `s.as_ptr()` is valid for `s.len()` bytes for the duration of this
    // call, which is all the C function needs -- it copies the bytes into its
    // own persistent allocation before returning.
    //
    // This is the one PHP call this module makes directly rather than
    // through `shim.c`, and it is sound to make from a live Rust frame for
    // the same reason `go_update_request_info` is: `persistent = 1` routes
    // through `__zend_malloc`, which on allocation failure prints "Out of
    // memory" and calls `exit(1)` -- it never reaches `zend_bailout()`, so
    // there is no `longjmp` here for a Rust frame to be exposed to. The
    // *request* allocator, the one that does bail out, plays no part in
    // interning.
    let ptr = unsafe {
        frankenrust_sys::frankenphp_init_persistent_string(s.as_ptr() as *const c_char, s.len())
    };
    InternedZendString(ptr)
}

// ---------------------------------------------------------------------------
// $_SERVER (cgi.go:43-148, frankenphp.h:82-119)
// ---------------------------------------------------------------------------

/// Owned buffers for one `frankenphp_register_server_vars` call. Upstream's
/// `addKnownVariablesToServer` (`cgi.go:47-148`) builds the FFI struct and
/// calls the C function in the same expression; splitting buffer computation
/// out like this makes it testable without a live PHP request and without
/// calling the real FFI function, which reads `frankenphp_strings` /
/// `main_thread_env` -- both NULL outside a booted PHP main thread (see this
/// issue's Acceptance notes).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ComputedServerVars {
    /// `REMOTE_ADDR` and `REMOTE_HOST` both read this -- upstream does no
    /// reverse DNS lookup (`cgi.go:88-89`), so the two are always identical
    /// and only need one buffer.
    remote_addr: Vec<u8>,
    remote_port: Vec<u8>,
    document_root: Vec<u8>,
    path_info: Vec<u8>,
    php_self: Vec<u8>,
    document_uri: Vec<u8>,
    script_filename: Vec<u8>,
    script_name: Vec<u8>,
    server_name: Vec<u8>,
    server_port: Vec<u8>,
    content_length: Vec<u8>,
    server_protocol: Vec<u8>,
    http_host: Vec<u8>,
    request_uri: Vec<u8>,
    /// `28 + len(request.Header) + len(fc.env) + lengthOfEnv` (`cgi.go:107`)
    /// -- a `zend_hash_extend` sizing hint, not a hard count. `fc.env`
    /// (prepared env) and the OS-env length are both out of scope for this
    /// issue (the former has no field on [`RequestContext`]; the latter is
    /// #10's `go_init_os_env`), so both terms are always 0 here -- named
    /// explicitly in [`compute_server_vars`] so the omission reads as
    /// deliberate. An undercount only costs one rehash.
    total_num_vars: usize,
}

/// Port of `addKnownVariablesToServer` (`cgi.go:47-148`), minus the actual
/// FFI call -- see [`ComputedServerVars`]'s doc comment. Returns `None` when
/// there is no request (upstream's `if fc.request != nil` guard,
/// `cgi.go:179`).
fn compute_server_vars(ctx: &RequestContext) -> Option<ComputedServerVars> {
    let request = ctx.request.as_ref()?;

    // Both REMOTE_ADDR and REMOTE_HOST -- see the field's doc comment.
    let (remote_addr, remote_port) = cgi::split_remote_addr(request.remote_addr.as_bytes());

    // TLS has no field on Request and is out of scope for this port (see
    // this module's `InternedStrings` doc comment), so the scheme is always
    // Http: SERVER_NAME/SERVER_PORT always fall back to CGI's port-80
    // default when Host carries no port of its own.
    let (server_name, server_port) =
        cgi::derive_server_name_and_port(request.host.as_bytes(), cgi::Scheme::Http);

    // request.Header.Get("Content-Length") (cgi.go:92-93): the raw header
    // text of the *first* Content-Length, never Request::content_length (see
    // that field's own doc comment for why the two legitimately disagree for
    // a chunked body) and never a join of every Content-Length header the
    // client happened to send.
    let content_length = request
        .headers
        .get_first("Content-Length")
        .unwrap_or_default()
        .to_vec();

    // PHP_SELF = SCRIPT_NAME + PATH_INFO (cgi.go:102).
    let mut php_self = ctx.script_name.clone();
    php_self.extend_from_slice(&ctx.path_info);

    // SERVER_PROTOCOL has no backing field: Request carries proto_major/
    // proto_minor (context.rs), not Go's pre-formatted `request.Proto`
    // (cgi.go:127), so this is the one $_SERVER value this module derives
    // rather than copies.
    let server_protocol = format!("HTTP/{}.{}", request.proto_major, request.proto_minor);

    // cgi.go:107. Named as three terms even though two are always zero --
    // see `total_num_vars`'s doc comment for why.
    const PREPARED_ENV_VARS: usize = 0;
    const OS_ENV_VARS: usize = 0;
    let total_num_vars = 28 + request.headers.iter().count() + PREPARED_ENV_VARS + OS_ENV_VARS;

    Some(ComputedServerVars {
        remote_addr,
        remote_port,
        document_root: ctx.document_root.clone().into_bytes(),
        path_info: ctx.path_info.clone(),
        php_self,
        document_uri: ctx.doc_uri.clone(),
        script_filename: ctx.script_filename.clone(),
        script_name: ctx.script_name.clone(),
        server_name,
        server_port,
        content_length,
        server_protocol: server_protocol.into_bytes(),
        http_host: request.host.clone().into_bytes(),
        request_uri: ctx.request_uri.clone(),
        total_num_vars,
    })
}

impl ComputedServerVars {
    /// Builds the by-value FFI struct `frankenphp_register_server_vars`
    /// consumes (`frankenphp.h:82-119`). Field order below matches the C
    /// declaration exactly -- see `frankenrust-sys/src/layout.rs`'s
    /// compile-time offset assertions, which this struct's layout is checked
    /// against.
    ///
    /// Borrows from `self`'s owned buffers (via [`c_buf`]) and the
    /// process-lifetime interned strings. The callee copies every field
    /// immediately (`ZVAL_STRINGL_FAST` via `frankenphp_register_trusted_var`,
    /// `frankenphp.c:1200-1218`, or `ZVAL_STR` for the three interned fields,
    /// `frankenphp.c:1257-1262`), so the returned struct only needs to
    /// outlive one call, not the request -- but the memory it borrows in the
    /// meantime is `self`'s, and `self` is always the copy installed in the
    /// [`RequestContext`] (see [`RequestContext::install_server_vars`]), so
    /// it outlives the call comfortably either way.
    fn as_ffi(&self) -> frankenphp_server_vars {
        let interned = interned_strings();
        let (remote_addr, remote_addr_len) = c_buf(&self.remote_addr);
        let (remote_port, remote_port_len) = c_buf(&self.remote_port);
        let (document_root, document_root_len) = c_buf(&self.document_root);
        let (path_info, path_info_len) = c_buf(&self.path_info);
        let (php_self, php_self_len) = c_buf(&self.php_self);
        let (document_uri, document_uri_len) = c_buf(&self.document_uri);
        let (script_filename, script_filename_len) = c_buf(&self.script_filename);
        let (script_name, script_name_len) = c_buf(&self.script_name);
        let (server_name, server_name_len) = c_buf(&self.server_name);
        let (server_port, server_port_len) = c_buf(&self.server_port);
        let (content_length, content_length_len) = c_buf(&self.content_length);
        let (server_protocol, server_protocol_len) = c_buf(&self.server_protocol);
        let (http_host, http_host_len) = c_buf(&self.http_host);
        let (request_uri, request_uri_len) = c_buf(&self.request_uri);

        frankenphp_server_vars {
            total_num_vars: self.total_num_vars,
            remote_addr,
            remote_addr_len,
            // Same buffer as remote_addr -- see that field's doc comment.
            remote_host: remote_addr,
            remote_host_len: remote_addr_len,
            remote_port,
            remote_port_len,
            document_root,
            document_root_len,
            path_info,
            path_info_len,
            php_self,
            php_self_len,
            document_uri,
            document_uri_len,
            script_filename,
            script_filename_len,
            script_name,
            script_name_len,
            server_name,
            server_name_len,
            server_port,
            server_port_len,
            content_length,
            content_length_len,
            server_protocol,
            server_protocol_len,
            http_host,
            http_host_len,
            request_uri,
            request_uri_len,
            // TLS is out of scope: always empty (frankenphp.c:1220-1268's
            // caller passes "" for ssl_cipher whenever request.TLS == nil).
            ssl_cipher: std::ptr::null_mut(),
            ssl_cipher_len: 0,
            request_scheme: interned.http_scheme.0,
            ssl_protocol: interned.empty.0,
            https: interned.empty.0,
        }
    }
}

/// The C descriptor array `shim.c` walks, held apart from the rest of
/// [`ServerVarsBatch`] so its `Send` impl is one obvious line rather than a
/// blanket claim over the whole struct.
///
/// `frankenrust_header_var` (`frankenrust_shim.h`) holds raw pointers, which
/// makes it `!Send`, and [`RequestContext`] must be `Send` because
/// [`crate::context::CONTEXT_SLOTS`] is a `static`.
struct HeaderVars(Vec<frankenrust_header_var>);

// SAFETY: every pointer in this vector targets either a heap buffer owned by
// the same `ServerVarsBatch` (`values`, `keys` -- see `build_server_vars_batch`,
// the only constructor) or a process-lifetime interned `zend_string` (which is
// already `Send`+`Sync`, see `InternedZendString`). Moving the batch to
// another thread moves `Vec` handles, never the heap bytes they point at, so
// every pointer stays valid across the move. None of the targets has interior
// mutability, and access is serialised anyway: the Rust side only ever
// reaches them under the slot `Mutex`, and the C side only from the PHP
// thread that owns that slot.
unsafe impl Send for HeaderVars {}

/// Owned backing store for one `go_register_server_variables` call: every key
/// and value the C side hands to PHP, plus the descriptor array pointing at
/// them.
///
/// ## Why this is owned by the context and not by a stack frame
///
/// See this module's doc comment for the bailout hazard this exists to
/// avoid. The buffers C reads cannot live in a Rust frame, because by the
/// time C reads them there is no Rust frame; they live in the
/// [`RequestContext`] instead, exactly like [`crate::context::RequestArena`]
/// backing `SG(request_info)`, and are reclaimed at the same point -- the
/// context slot being cleared or replaced (`context.rs:1021-1028`'s reclaim
/// point, restated on the `server_vars` field this batch is installed into).
/// A bailout partway through registration therefore leaks nothing: the
/// memory has an owner the `longjmp` cannot skip past.
pub struct ServerVarsBatch {
    known: ComputedServerVars,
    /// `", "`-joined header values (`cgi.go:153`, `:161`), one per entry of
    /// `headers`, in the same order.
    values: Vec<Vec<u8>>,
    /// NUL-terminated `HTTP_*` keys for the headers that miss the interned
    /// table (`phpheaders.go:126`). Sparse -- only slow-path headers push
    /// here.
    keys: Vec<Vec<u8>>,
    headers: HeaderVars,
}

/// Which of `frankenrust_header_var`'s two key forms a header resolved to,
/// recorded during a first pass so that pointers are only taken once
/// `values` and `keys` have stopped growing (see `build_server_vars_batch`).
enum HeaderKey {
    /// One of the ~101 pre-interned keys -- the `frankenphp_register_known_variable`
    /// fast path (`cgi.go:152-155`).
    Interned(InternedZendString),
    /// Index into `ServerVarsBatch::keys` -- the `frankenphp_register_variable_safe`
    /// slow path (`cgi.go:158-163`).
    Mangled(usize),
}

/// Port of `go_register_server_variables`'s Rust-side half (`cgi.go:174-188`):
/// everything up to, but not including, the calls into PHP.
///
/// `None` when there is no request, matching upstream's `if fc.request != nil`
/// guard (`cgi.go:179`), which gates *both* the known variables and the
/// headers.
fn build_server_vars_batch(ctx: &RequestContext) -> Option<ServerVarsBatch> {
    let known = compute_server_vars(ctx)?;
    let request = ctx.request.as_ref()?;
    let interned = interned_strings();

    let header_count = request.headers.iter().count();
    let mut values: Vec<Vec<u8>> = Vec::with_capacity(header_count);
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut key_forms: Vec<HeaderKey> = Vec::with_capacity(header_count);

    for (name, header_values) in request.headers.iter() {
        values.push(cgi::join_header_values(header_values));
        match interned.common_headers.get(name) {
            Some(interned_key) => key_forms.push(HeaderKey::Interned(*interned_key)),
            None => {
                keys.push(cgi::uncommon_header_key(name.as_bytes()));
                key_forms.push(HeaderKey::Mangled(keys.len() - 1));
            }
        }
    }

    // Second pass, now that neither `values` nor `keys` will be pushed to
    // again. (Their *inner* buffers would survive further growth of the
    // outer `Vec` anyway -- that is the same address-stability property the
    // arena relies on -- but taking the pointers only once nothing can move
    // is the invariant that is easy to keep true as this function is
    // edited.)
    let headers = key_forms
        .iter()
        .zip(values.iter())
        .map(|(key_form, value)| {
            let (value_ptr, value_len) = c_buf(value);
            frankenrust_header_var {
                known_key: match key_form {
                    HeaderKey::Interned(key) => key.0,
                    HeaderKey::Mangled(_) => std::ptr::null_mut(),
                },
                key: match key_form {
                    HeaderKey::Interned(_) => std::ptr::null_mut(),
                    HeaderKey::Mangled(index) => keys[*index].as_ptr() as *mut c_char,
                },
                value: value_ptr,
                value_len,
            }
        })
        .collect();

    Some(ServerVarsBatch {
        known,
        values,
        keys,
        headers: HeaderVars(headers),
    })
}

impl ServerVarsBatch {
    /// The by-value view `shim.c` reads, per `frankenrust_shim.h`.
    ///
    /// Every pointer in the returned struct borrows from `self` (or from the
    /// process-lifetime interned strings), so it is only meaningful while
    /// `self` is alive -- which is why the only caller installs `self` into
    /// the [`RequestContext`] first and hands C the result of calling this on
    /// the *installed* copy ([`RequestContext::install_server_vars`]).
    pub fn as_c_batch(&self) -> frankenrust_server_vars_batch {
        // `values` and `keys` exist to *own* what the descriptors point at:
        // their contents are only ever read from C, through those pointers,
        // which is why nothing in Rust reads them. Checking the shape here
        // pins the one-descriptor-per-value invariant `build_server_vars_batch`
        // establishes -- if a future edit pushed to one and not the other,
        // `num_headers` below would walk off the end of the C array.
        debug_assert_eq!(self.headers.0.len(), self.values.len());
        debug_assert!(self.keys.len() <= self.values.len());

        frankenrust_server_vars_batch {
            vars: self.known.as_ffi(),
            headers: self.headers.0.as_ptr(),
            num_headers: self.headers.0.len(),
        }
    }
}

/// The Rust half of `go_register_server_variables` (`cgi.go:174-188`), called
/// from `shim.c` before it makes its first Zend call. Declared in
/// `crates/frankenrust-sys/include/frankenrust_shim.h`.
///
/// Returns `false` -- leaving `*out` untouched, so C registers nothing -- when
/// there is no context for `thread_index`, or the context has no request. The
/// latter is upstream's `if fc.request != nil` guard (`cgi.go:179`), which
/// gates both the known variables and the headers.
///
/// Two properties this function must keep, both of which are why it exists
/// separately from `shim.c`'s entry point rather than being inlined into it:
///
/// 1. **It makes no call into PHP.** It therefore cannot bail out, so no
///    `longjmp` can cross this frame. Holding the slot lock for the whole
///    body is safe *because* of that, and would not be otherwise -- see
///    [`crate::context::ContextSlots`]'s "one rule for callers": a leaked
///    slot guard wedges the thread's own crash-recovery path
///    (`frankenphp.c:1592` -> `go_frankenphp_after_script_execution` clears
///    this very slot).
/// 2. **It returns before the registration starts,** and hands out only
///    pointers into memory the [`RequestContext`] owns -- installed into its
///    `server_vars` field by [`RequestContext::install_server_vars`], and
///    reclaimed when that context's slot is cleared or replaced
///    (`context.rs:1021-1028`) -- so a bailout during the registration in
///    `shim.c` leaks nothing and dangles nothing.
///
/// # Safety
/// Must be called only from `shim.c`'s `go_register_server_variables`, which C
/// calls from `frankenphp_register_variables()` (`frankenphp.c:1371-1383`) on
/// the PHP thread that owns `thread_index`. `out` must be a writable,
/// suitably aligned `frankenrust_server_vars_batch` (`shim.c` passes the
/// address of one of its own locals) that stays alive for this call.
///
/// The pointers written into `*out` are valid until `thread_index`'s context
/// slot is cleared or replaced -- which, on both of upstream's reclaim paths,
/// happens strictly after C is finished with them: regular mode clears at
/// `go_frankenphp_after_script_execution` (`threadregular.go:129-133`), worker
/// mode at `go_frankenphp_finish_worker_request` (`threadworker.go:314-318`),
/// and both run long after `frankenphp_register_variables` has returned.
///
/// This function must not panic across the `extern "C"` boundary: a
/// panic unwinding into `shim.c` is undefined behaviour (`docs/PORTING-NOTES.md`;
/// project-wide unwind guarding is #78's, tracked separately, not fixed here).
/// Every fallible step here is handled with `Option`/`if`, never `.unwrap()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frankenrust_collect_server_vars(
    thread_index: usize,
    out: *mut frankenrust_server_vars_batch,
) -> bool {
    if out.is_null() {
        return false;
    }

    CONTEXT_SLOTS.with_context_mut(thread_index, |slot| {
        // Upstream dereferences `thread.frankenPHPContext()` unconditionally
        // here (cgi.go:176-177) and would itself panic on a nil context --
        // this call site is only ever reached from inside
        // php_request_startup's $_SERVER population, by which point a
        // context always exists. A panic here would be UB across the
        // `extern "C"` boundary, so on the should-be-unreachable case we log
        // and register nothing rather than abort the whole process for it.
        let Some(ctx) = slot else {
            eprintln!(
                "frankenrust: go_register_server_variables: no RequestContext for thread {thread_index}"
            );
            return false;
        };

        let Some(batch) = build_server_vars_batch(ctx) else {
            return false;
        };

        // Installing before taking the C view is what makes the pointers
        // outlive this frame: they target the context's copy, not the local.
        //
        // Prepared-env merge (cgi.go:185-187,
        // frankenphp_merge_with_prepared_env) is out of scope: `fc.env`
        // (PreparedEnv) is not part of this issue's RequestContext -- see
        // this issue's "out of scope" section.
        let c_batch = ctx.install_server_vars(batch);

        // SAFETY: `out` is non-null (checked above) and, per this function's
        // contract, points at a writable, aligned, live
        // `frankenrust_server_vars_batch` owned by the C caller's frame.
        // `write` (not `*out = `) because the destination is C's
        // uninitialised local: an assignment would first *drop* whatever is
        // nominally there. `frankenrust_server_vars_batch` is plain old data
        // with no drop glue, so the two are equivalent today -- `write` is
        // what states the intent and stays correct if that ever changes.
        unsafe { out.write(c_batch) };
        true
    })
}

// ---------------------------------------------------------------------------
// sapi_request_info (cgi.go:284-324)
// ---------------------------------------------------------------------------

/// Mirrors `cStringHTTPMethods` (`cgi.go:31-41`), but needs no allocation at
/// all: these are `'static` C-string literals (stable since Rust 1.77),
/// unlike upstream's one-time `C.CString` leak.
fn cached_method_cstr(method: &str) -> Option<*const c_char> {
    let cstr: &'static std::ffi::CStr = match method {
        "GET" => c"GET",
        "HEAD" => c"HEAD",
        "POST" => c"POST",
        "PUT" => c"PUT",
        "DELETE" => c"DELETE",
        "CONNECT" => c"CONNECT",
        "OPTIONS" => c"OPTIONS",
        "TRACE" => c"TRACE",
        "PATCH" => c"PATCH",
        _ => return None,
    };
    Some(cstr.as_ptr())
}

/// Port of `go_update_request_info`'s body (`cgi.go:284-324`), minus the
/// `//export` C-ABI wrapper. Writes only through `info` and the context's
/// arena -- no call into PHP, so (unlike `go_register_server_variables`) this
/// one can stay entirely in Rust; see this callback's own doc comment.
///
/// Returns the `Authorization` header value, arena-allocated, or NULL if
/// absent or empty -- `frankenphp.c:358` feeds this straight to
/// `php_handle_auth_data`, which neither retains nor frees it.
fn update_request_info(ctx: &mut RequestContext, info: &mut sapi_request_info) -> *mut c_char {
    let Some(request) = ctx.request.as_ref() else {
        return std::ptr::null_mut();
    };

    // Upstream's `if len(fc.env) != 0 { registerPreparedEnv(fc.env) }`
    // (cgi.go:295-297) has no counterpart here: `fc.env` (PreparedEnv) is not
    // part of this issue's `RequestContext` -- see the "out of scope" note on
    // `registerPreparedEnv` in this issue's spec. `RequestContext` has no
    // `env` field, so there is nothing to check.
    let method = request.method.clone();
    let query = request.query.clone();
    let content_length = request.content_length;
    // request.Header.Get(...) at cgi.go:306 and :316 -- the first value of
    // each, never a join. A joined Content-Type matches no registered POST
    // reader, and a joined Authorization is un-decodable garbage for
    // `php_handle_auth_data` (frankenphp.c:358).
    //
    // Content-Type: cgi.go:307 tests `!= ""`, not presence -- a header sent
    // present-but-empty must leave content_type NULL, same as an absent one.
    let content_type = request
        .headers
        .get_first("Content-Type")
        .filter(|value| !value.is_empty())
        .map(<[u8]>::to_vec);
    // Authorization: cgi.go:319-322 returns nil for "" as well as absent.
    let authorization = request
        .headers
        .get_first("Authorization")
        .filter(|value| !value.is_empty())
        .map(<[u8]>::to_vec);
    let path_info_is_empty = ctx.path_info.is_empty();
    // sanitizedPathJoin(fc.documentRoot, fc.pathInfo) (cgi.go:296) -- not
    // path_info alone.
    let path_translated = (!path_info_is_empty)
        .then(|| cgi::sanitized_path_join(ctx.document_root.as_bytes(), &ctx.path_info));
    let request_uri = ctx.request_uri.clone();
    let proto_num = i32::from(request.proto_major) * 1000 + i32::from(request.proto_minor);

    info.request_method = cached_method_cstr(&method)
        .unwrap_or_else(|| ctx.arena.alloc(method.as_bytes()).cast_const());
    info.query_string = ctx.arena.alloc(&query);
    info.content_length = content_length as frankenrust_sys::zend_long;
    if let Some(content_type) = content_type {
        info.content_type = ctx.arena.alloc(&content_type).cast_const();
    }
    if let Some(path_translated) = path_translated {
        info.path_translated = ctx.arena.alloc(&path_translated);
    }
    info.request_uri = ctx.arena.alloc(&request_uri);
    info.proto_num = proto_num;

    match authorization {
        Some(bytes) => ctx.arena.alloc(&bytes),
        None => std::ptr::null_mut(),
    }
}

/// `frankenphp.c:355`, inside `frankenphp_update_request_context()`, called
/// at the top of every request (`frankenphp.c:1509`) and every worker
/// request (`frankenphp.c:563`), strictly before `php_request_startup()`.
///
/// # Safety
/// Must be called only from `php_thread()` on the OS thread that owns
/// `thread_index`, with `info` either NULL or pointing at that thread's own
/// `SG(request_info)` (TSRM-resident, so exclusively this thread's) for the
/// duration of this call -- again the contract C already provides: the only
/// caller is `frankenphp_update_request_context()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_update_request_info(
    thread_index: usize,
    info: *mut sapi_request_info,
) -> *mut c_char {
    if info.is_null() {
        return std::ptr::null_mut();
    }

    // Runs under the slot lock, and must: it appends to the context's arena,
    // which the context owns. That is sound for the same reason
    // `frankenrust_collect_server_vars` above is -- `update_request_info`
    // makes no call into PHP at all, only writes plain fields of
    // `SG(request_info)` and allocates through Rust, so there is no
    // `zend_bailout()` that could `longjmp` past the guard's destructor.
    CONTEXT_SLOTS.with_context_mut(thread_index, |slot| {
        let Some(ctx) = slot else {
            eprintln!(
                "frankenrust: go_update_request_info: no RequestContext for thread {thread_index}"
            );
            return std::ptr::null_mut();
        };
        // SAFETY: `info` is `&SG(request_info)` (frankenphp.c:355), passed by
        // the PHP thread that owns `thread_index` and exclusively ours for
        // the duration of this call -- it is TSRM-resident state on the
        // calling thread, per this function's contract. `info` was just
        // checked non-null above.
        let info = unsafe { &mut *info };
        update_request_info(ctx, info)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::time::Duration;

    use crate::context::{CompletionSignal, Request};

    fn install(thread_index: usize, document_root: &str, request: Request) {
        CONTEXT_SLOTS.set(
            thread_index,
            RequestContext::new(
                document_root.to_string(),
                None,
                Some(request),
                CompletionSignal::none(),
            ),
        );
    }

    fn install_empty(thread_index: usize) {
        CONTEXT_SLOTS.set(
            thread_index,
            RequestContext::new(String::new(), None, None, CompletionSignal::none()),
        );
    }

    /// Installs a context on `thread_index` and returns the batch `shim.c`
    /// would have received.
    fn collect(thread_index: usize) -> Option<frankenrust_server_vars_batch> {
        let mut batch = frankenrust_server_vars_batch::default();
        // SAFETY: stands in for `shim.c`'s call. `&mut batch` is a writable,
        // aligned, live `frankenrust_server_vars_batch` on this frame, and
        // whatever context the caller installed on `thread_index` lives in
        // that slot for the rest of the test, so the pointers written into
        // it stay valid.
        let filled = unsafe { frankenrust_collect_server_vars(thread_index, &mut batch) };
        filled.then_some(batch)
    }

    // SAFETY (every raw-pointer dereference below in this module -- `&*ptr`,
    // `slice::from_raw_parts`, `CStr::from_ptr` alike): the context that
    // produced the pointer is still installed in its slot for the duration of
    // the test (cleared only at the very end, or in a couple of tests, right
    // after the read), so every pointer read back here is live -- that
    // liveness is exactly the property under test in each case.

    fn bytes_at(ptr: *const c_char, len: usize) -> &'static [u8] {
        if ptr.is_null() {
            assert_eq!(
                len, 0,
                "a non-null-paired length must be 0 (empty-value rule)"
            );
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }
    }

    #[test]
    fn phase_one_computes_every_documented_server_var() {
        const THREAD_INDEX: usize = 60;

        let request = Request::new("GET", "/index.php/extra")
            .with_query(b"a=1".to_vec())
            .with_host("example.com:9000")
            .with_remote_addr("10.0.0.5:4321")
            .with_proto(1, 1)
            .with_header("Content-Length", b"42".to_vec());
        install(THREAD_INDEX, "/var/www", request);
        CONTEXT_SLOTS.with_context_mut(THREAD_INDEX, |slot| {
            let ctx = slot.expect("just installed");
            ctx.script_name = b"/index.php".to_vec();
            ctx.path_info = b"/extra".to_vec();
            ctx.doc_uri = b"/index.php".to_vec();
            ctx.script_filename = b"/var/www/index.php".to_vec();
        });

        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        let vars = batch.vars;

        assert_eq!(
            bytes_at(vars.http_host, vars.http_host_len),
            b"example.com:9000"
        );
        assert_eq!(
            bytes_at(vars.server_name, vars.server_name_len),
            b"example.com"
        );
        assert_eq!(bytes_at(vars.server_port, vars.server_port_len), b"9000");
        assert_eq!(
            bytes_at(vars.php_self, vars.php_self_len),
            b"/index.php/extra"
        );
        assert_eq!(
            bytes_at(vars.server_protocol, vars.server_protocol_len),
            b"HTTP/1.1"
        );
        assert_eq!(
            bytes_at(vars.content_length, vars.content_length_len),
            b"42"
        );
        assert_eq!(
            bytes_at(vars.remote_addr, vars.remote_addr_len),
            b"10.0.0.5"
        );
        assert_eq!(
            bytes_at(vars.remote_host, vars.remote_host_len),
            b"10.0.0.5"
        );
        assert_eq!(bytes_at(vars.remote_port, vars.remote_port_len), b"4321");
        assert_eq!(
            bytes_at(vars.document_root, vars.document_root_len),
            b"/var/www"
        );
        assert!(vars.total_num_vars >= 28);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn server_port_defaults_to_80_when_host_carries_no_port() {
        const THREAD_INDEX: usize = 61;

        let request = Request::new("GET", "/").with_host("example.com");
        install(THREAD_INDEX, "/var/www", request);

        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        assert_eq!(
            bytes_at(batch.vars.server_port, batch.vars.server_port_len),
            b"80"
        );
        assert_eq!(
            bytes_at(batch.vars.server_name, batch.vars.server_name_len),
            b"example.com"
        );

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn an_absent_content_length_header_is_a_null_pointer() {
        const THREAD_INDEX: usize = 62;

        let request = Request::new("GET", "/").with_host("example.com");
        install(THREAD_INDEX, "/var/www", request);

        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        assert!(batch.vars.content_length.is_null());
        assert_eq!(batch.vars.content_length_len, 0);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn a_present_but_empty_header_registers_as_a_null_pointer() {
        const THREAD_INDEX: usize = 63;

        let request = Request::new("GET", "/")
            .with_host("example.com")
            .with_header("X-Empty", b"".to_vec());
        install(THREAD_INDEX, "/var/www", request);

        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        assert_eq!(batch.num_headers, 1);
        let header = unsafe { &*batch.headers };
        assert!(header.value.is_null(), "empty header value must be NULL");
        assert_eq!(header.value_len, 0);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn collection_reports_nothing_without_a_request() {
        const THREAD_INDEX: usize = 64;

        install_empty(THREAD_INDEX);
        let mut batch = frankenrust_server_vars_batch::default();
        // SAFETY: `&mut batch` is writable, aligned and live; the context
        // installed above has no request, which is the case under test.
        let filled = unsafe { frankenrust_collect_server_vars(THREAD_INDEX, &mut batch) };

        assert!(
            !filled,
            "no request on the context: C must register nothing rather than read an \
             untouched batch"
        );
        assert_eq!(batch.num_headers, 0, "the batch must be left untouched");

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn collection_reports_nothing_without_a_context() {
        // Reserved to this test alone -- no `set` call targets this index.
        const THREAD_INDEX: usize = 65;

        let mut batch = frankenrust_server_vars_batch::default();
        // SAFETY: `&mut batch` is writable, aligned and live; no context is
        // installed on this index, which is the case under test.
        let filled = unsafe { frankenrust_collect_server_vars(THREAD_INDEX, &mut batch) };

        assert!(!filled);
        assert_eq!(batch.num_headers, 0);
    }

    #[test]
    fn collection_rejects_a_null_out_pointer() {
        const THREAD_INDEX: usize = 66;

        install(THREAD_INDEX, "/var/www", Request::new("GET", "/index.php"));

        // SAFETY: the null case is precisely what is under test, and the
        // function documents that it checks before writing.
        let filled = unsafe { frankenrust_collect_server_vars(THREAD_INDEX, std::ptr::null_mut()) };
        assert!(!filled);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    /// The reviewed defect this pins down: the callback used to hold the
    /// context slot's `Mutex` (and the slot table's `RwLock` read guard)
    /// across `frankenphp_register_server_vars` and one
    /// `frankenphp_register_known_variable`/`_variable_safe` per header. Any
    /// of those can exhaust `memory_limit` -- ordinary in worker mode, where
    /// the resident worker script counts against the same budget -- and
    /// `zend_error_noreturn(E_ERROR, ...)` ends in `zend_bailout()`, a
    /// `longjmp` that runs no Rust destructor. The guards would be leaked and
    /// the slot locked forever, right before C's crash-recovery path
    /// (`frankenphp.c:1592` -> `go_frankenphp_after_script_execution`) tries
    /// to clear that very slot.
    ///
    /// The registration now happens in `shim.c` *after* this function has
    /// returned, so the guards cannot still be held -- but "cannot" is a
    /// claim about this function's return, which is exactly what a test can
    /// check. The probe thread stands in for the post-bailout cleanup; if any
    /// guard outlived the call it would block forever and this test would
    /// fail on the timeout.
    #[test]
    fn the_slot_is_free_once_collection_returns() {
        const THREAD_INDEX: usize = 67;

        install(THREAD_INDEX, "/var/www", Request::new("GET", "/index.php"));
        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        assert_eq!(
            bytes_at(batch.vars.script_name, batch.vars.script_name_len),
            b""
        );

        let (done_tx, done_rx) = mpsc::channel();
        let probe = std::thread::spawn(move || {
            CONTEXT_SLOTS.clear(THREAD_INDEX);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the context slot must be free once collection returns: the C caller goes \
             straight on to call into PHP, and a zend_bailout() out of those calls \
             never runs Rust destructors, so a guard still held here would wedge the \
             slot permanently"
        );
        probe.join().expect("probe thread panicked");
    }

    /// Forces at least one `Vec` reallocation of `keys` (it starts at
    /// capacity 0 and grows one push at a time, unlike `values`/`key_forms`,
    /// which are sized upfront) by installing more than 32 distinct headers,
    /// mixing pre-interned (fast-path) and uncommon (slow-path) keys, then
    /// reads every `frankenrust_header_var` back through the C view returned
    /// after installation -- pinning that no pointer in the batch was taken
    /// before the owning buffers stopped growing.
    #[test]
    fn header_pointers_survive_reallocation_and_install() {
        const THREAD_INDEX: usize = 68;

        let mut request = Request::new("GET", "/index.php").with_host("example.com");
        // (name, value, is a pre-interned common header)
        let mut expected: Vec<(String, String, bool)> = Vec::new();
        for (name, _) in cgi::COMMON_HEADERS.iter().take(20) {
            let value = format!("v-{name}");
            request = request.with_header(name, value.clone().into_bytes());
            expected.push((name.to_string(), value, true));
        }
        for i in 0..20 {
            let name = format!("X-Custom-{i}");
            let value = format!("custom-value-{i}");
            request = request.with_header(&name, value.clone().into_bytes());
            expected.push((name, value, false));
        }
        assert!(expected.len() >= 32, "must force at least one reallocation");

        install(THREAD_INDEX, "/var/www", request);
        let batch = collect(THREAD_INDEX).expect("a context with a request is installed");
        assert_eq!(batch.num_headers, expected.len());

        // SAFETY: `batch.headers` was written by `frankenrust_collect_server_vars`
        // above and targets memory owned by `THREAD_INDEX`'s still-installed
        // `RequestContext`, so it is valid for `batch.num_headers` reads for
        // the rest of this test (cleared only at the very end).
        let headers = unsafe { std::slice::from_raw_parts(batch.headers, batch.num_headers) };
        for (name, value, is_common) in &expected {
            // Every value in this test is unique, so matching on value alone
            // is enough to locate the descriptor for `name`.
            let header = headers
                .iter()
                .find(|h| bytes_at(h.value, h.value_len) == value.as_bytes())
                .unwrap_or_else(|| panic!("missing header {name} (value {value})"));

            assert_eq!(bytes_at(header.value, header.value_len), value.as_bytes());
            if *is_common {
                assert!(
                    !header.known_key.is_null(),
                    "{name} should use the fast path"
                );
                assert!(
                    header.key.is_null(),
                    "{name} must not also carry a slow-path key"
                );
            } else {
                assert!(
                    header.known_key.is_null(),
                    "{name} should use the slow path"
                );
                assert!(!header.key.is_null(), "{name} should use the slow path");
                // SAFETY: `header.key` is the NUL-terminated slow-path key
                // this same context installed; still alive per this test's
                // outer SAFETY comment.
                assert_eq!(
                    unsafe { std::ffi::CStr::from_ptr(header.key) }.to_bytes(),
                    &cgi::uncommon_header_key(name.as_bytes())[..name.len() + 5]
                );
            }
        }

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    // -----------------------------------------------------------------------
    // go_update_request_info
    // -----------------------------------------------------------------------

    fn update_info(thread_index: usize) -> (sapi_request_info, *mut c_char) {
        let mut info = sapi_request_info::default();
        // SAFETY: stands in for frankenphp_update_request_context()'s call.
        // `&mut info` is writable, aligned and live for the duration of the
        // call, and the context installed on `thread_index` outlives it.
        let auth = unsafe { go_update_request_info(thread_index, &mut info) };
        (info, auth)
    }

    #[test]
    fn path_translated_is_null_when_path_info_is_empty() {
        const THREAD_INDEX: usize = 69;

        install(THREAD_INDEX, "/var/www", Request::new("GET", "/index.php"));
        let (info, _) = update_info(THREAD_INDEX);
        assert!(info.path_translated.is_null());

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn path_translated_is_sanitized_join_when_path_info_is_present() {
        const THREAD_INDEX: usize = 70;

        install(
            THREAD_INDEX,
            "/var/www",
            Request::new("GET", "/index.php/more"),
        );
        CONTEXT_SLOTS.with_context_mut(THREAD_INDEX, |slot| {
            slot.expect("just installed").path_info = b"/more".to_vec();
        });

        let (info, _) = update_info(THREAD_INDEX);
        assert!(!info.path_translated.is_null());
        let translated = unsafe { std::ffi::CStr::from_ptr(info.path_translated) }.to_bytes();
        assert_eq!(
            translated,
            cgi::sanitized_path_join(b"/var/www", b"/more").as_slice()
        );

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn content_length_minus_one_passes_through_verbatim() {
        const THREAD_INDEX: usize = 71;

        install(
            THREAD_INDEX,
            "/var/www",
            Request::new("GET", "/index.php").with_content_length(-1),
        );
        let (info, _) = update_info(THREAD_INDEX);
        assert_eq!(info.content_length, -1);

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn content_type_is_null_for_a_present_but_empty_header() {
        const THREAD_INDEX: usize = 72;

        install(
            THREAD_INDEX,
            "/var/www",
            Request::new("GET", "/index.php").with_header("Content-Type", b"".to_vec()),
        );
        let (info, _) = update_info(THREAD_INDEX);
        assert!(info.content_type.is_null());

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn authorization_is_null_for_absent_and_empty_headers() {
        const ABSENT: usize = 73;
        const EMPTY: usize = 74;

        install(ABSENT, "/var/www", Request::new("GET", "/index.php"));
        let (_, auth_absent) = update_info(ABSENT);
        assert!(auth_absent.is_null());
        CONTEXT_SLOTS.clear(ABSENT);

        install(
            EMPTY,
            "/var/www",
            Request::new("GET", "/index.php").with_header("Authorization", b"".to_vec()),
        );
        let (_, auth_empty) = update_info(EMPTY);
        assert!(auth_empty.is_null());
        CONTEXT_SLOTS.clear(EMPTY);
    }

    #[test]
    fn authorization_is_returned_when_present() {
        const THREAD_INDEX: usize = 75;

        install(
            THREAD_INDEX,
            "/var/www",
            Request::new("GET", "/index.php").with_header("Authorization", b"Bearer xyz".to_vec()),
        );
        let (_, auth) = update_info(THREAD_INDEX);
        assert!(!auth.is_null());
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(auth) }.to_bytes(),
            b"Bearer xyz"
        );

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }

    #[test]
    fn update_request_info_returns_null_without_a_request() {
        const THREAD_INDEX: usize = 76;

        install_empty(THREAD_INDEX);
        let (info, auth) = update_info(THREAD_INDEX);
        assert!(auth.is_null());
        assert!(info.request_method.is_null());

        CONTEXT_SLOTS.clear(THREAD_INDEX);
    }
}
