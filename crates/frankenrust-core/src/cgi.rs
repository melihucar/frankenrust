//! Port of `vendor/frankenphp/cgi.go`: CGI path splitting, `$_SERVER`
//! population, and `sapi_request_info` population. Mirrors upstream's own
//! file boundary -- cgi.go mixes pure Go helpers with the cgo-calling
//! functions that use them, and this file does the same, with
//! `callbacks/servervars.rs` holding only the thin `#[no_mangle]` `//export`
//! wrappers (issue #11's module-layout split).

use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::OnceLock;

use frankenrust_sys::{
    frankenphp_server_vars, frankenrust_header_var, frankenrust_server_vars_batch,
    sapi_request_info, zend_string,
};

use crate::context::{RequestContext, Scheme};

// ---------------------------------------------------------------------------
// Path helpers (cgi.go:191-392)
// ---------------------------------------------------------------------------

/// Port of `ensureLeadingSlash` (`cgi.go:378-384`).
pub fn ensure_leading_slash(path: &str) -> String {
    if path.is_empty() || path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// `ErrInvalidSplitPath` (`requestoptions.go:21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSplitPath;

impl std::fmt::Display for InvalidSplitPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("split path contains non-ASCII characters")
    }
}

impl std::error::Error for InvalidSplitPath {}

/// Port of `WithRequestSplitPath` (`requestoptions.go:86-113`): reject any
/// entry containing a byte >= 0x80, and ASCII-lower-case the rest.
///
/// This runs at *configuration* time, not per request, and it is what makes
/// [`split_pos`]'s one-sided fold correct: `split_pos` folds only the bytes
/// of the path it is searching, and compares them against the split entries
/// verbatim. An un-normalised entry is fail-*closed* rather than a bypass --
/// ".PHP" or an entry with a non-ASCII byte simply never matches anything --
/// but silently never matching is not the configured behaviour, so like
/// upstream we normalise what we can and refuse what we cannot.
pub fn normalize_split_path(split_path: Vec<String>) -> Result<Vec<String>, InvalidSplitPath> {
    split_path
        .into_iter()
        .map(|split| {
            if !split.is_ascii() {
                return Err(InvalidSplitPath);
            }
            Ok(split.to_ascii_lowercase())
        })
        .collect()
}

/// Port of `splitPos` (`cgi.go:238-279`): a hand-rolled, ASCII-only,
/// case-insensitive substring search. Bytes >= 0x80 never match, by design
/// (see upstream's comment and GHSA-3g8v-8r37-cgjm / GHSA-v4h7-cj44-8fc8) --
/// do not "fix" this to be Unicode-aware, that is exactly the vulnerability
/// class it exists to avoid.
///
/// `split_path`'s entries are assumed ASCII and already lower-cased, which is
/// what [`normalize_split_path`] guarantees (upstream leans on
/// `WithRequestSplitPath` for the same guarantee).
pub fn split_pos(path: &str, split_path: &[String]) -> isize {
    if split_path.is_empty() {
        return 0;
    }

    let path = path.as_bytes();
    let path_len = path.len();

    for split in split_path {
        let split = split.as_bytes();
        let split_len = split.len();
        if split_len == 0 || split_len > path_len {
            continue;
        }

        for i in 0..=(path_len - split_len) {
            let mut matched = true;
            for j in 0..split_len {
                let mut c = path[i + j];
                if c >= 0x80 {
                    matched = false;
                    break;
                }
                if c.is_ascii_uppercase() {
                    c += b'a' - b'A';
                }
                if c != split[j] {
                    matched = false;
                    break;
                }
            }
            if matched {
                return (i + split_len) as isize;
            }
        }
    }

    -1
}

/// Port of Go's real `net.SplitHostPort`, used only for deriving SERVER_NAME
/// (`cgi.go:72`: `net.SplitHostPort(request.Host)`). This is **not** the
/// same function as [`split_remote_addr`] below, which is upstream's own
/// hand-rolled, never-erroring variant used for REMOTE_ADDR/REMOTE_PORT
/// only (`cgi.go:354-374`). `None` corresponds to Go's returned `err != nil`.
fn split_host_port(hostport: &str) -> Option<(String, String)> {
    let bytes = hostport.as_bytes();
    let i = hostport.rfind(':')?;

    let host;
    let j;
    let k;
    if bytes.first() == Some(&b'[') {
        let end = hostport.find(']')?;
        if end + 1 == bytes.len() {
            return None; // missing port
        } else if end + 1 == i {
            // expected shape: "[...]:port"
        } else {
            return None; // too many colons / missing port
        }
        host = hostport[1..end].to_string();
        j = 1;
        k = end + 1;
    } else {
        host = hostport[..i].to_string();
        if host.contains(':') {
            return None; // too many colons in address
        }
        j = 0;
        k = 0;
    }

    if hostport[j..].contains('[') {
        return None;
    }
    if hostport[k..].contains(']') {
        return None;
    }

    let port = hostport[i + 1..].to_string();
    Some((host, port))
}

/// Port of `splitRemoteAddr` (`cgi.go:354-374`): must never panic, since a
/// panic here would unwind out of an `extern "C"` callback and abort the
/// process on a malformed `RemoteAddr`.
pub fn split_remote_addr(remote_addr: &str) -> (String, String) {
    if let Some((host, port)) = split_host_port(remote_addr) {
        return (host, port);
    }

    let mut ip;
    let mut port = String::new();
    if let Some(idx) = remote_addr.rfind(':') {
        ip = remote_addr[..idx].to_string();
        port = remote_addr[idx + 1..].to_string();
    } else {
        ip = remote_addr.to_string();
    }

    if ip.len() >= 2 && ip.as_bytes()[0] == b'[' && ip.as_bytes()[ip.len() - 1] == b']' {
        ip = ip[1..ip.len() - 1].to_string();
    }

    (ip, port)
}

/// Port of Go's `path/filepath.Clean` for Unix (single `/` separator; the
/// only separator `frankenrust-sys/build.rs` supports -- it panics on
/// Windows). Operates byte-wise, which is safe even on non-UTF8-clean input:
/// `.`/`/` are ASCII and never appear as a UTF-8 continuation byte.
fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }

    let bytes = path.as_bytes();
    let n = bytes.len();
    let rooted = bytes[0] == b'/';
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut dotdot;
    let mut r;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    } else {
        r = 0;
        dotdot = 0;
    }

    while r < n {
        // A literal '/' (multiple slashes collapsing to one) and a lone "."
        // path segment (elided entirely) both just advance past one
        // character -- two logically distinct cases from Go's Clean that
        // happen to take the same action, merged here so clippy doesn't see
        // two branches with identical bodies.
        if bytes[r] == b'/' || (bytes[r] == b'.' && (r + 1 == n || bytes[r + 1] == b'/')) {
            r += 1;
        } else if bytes[r] == b'.'
            && r + 1 < n
            && bytes[r + 1] == b'.'
            && (r + 2 == n || bytes[r + 2] == b'/')
        {
            r += 2;
            if out.len() > dotdot {
                // Go walks a write cursor `out.w` back over the segment and
                // stops on the separator *at* `out.w` -- i.e. the byte one
                // past the content it keeps, which is therefore dropped along
                // with the segment. Popping and testing the last *remaining*
                // byte instead stops one byte early and leaves the separator
                // behind ("/var/www/.." -> "/var/" rather than "/var"), so
                // the cursor is modelled explicitly here: `w` indexes bytes
                // that are still in `out` until the final `truncate`.
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && bytes[r] != b'/' {
                out.push(bytes[r]);
                r += 1;
            }
        }
    }

    if out.is_empty() {
        return ".".to_string();
    }
    // `out` only ever contains bytes copied verbatim from `path` (a valid
    // `&str`) plus ASCII '.'/'/' pushed by this function, so it is valid
    // UTF-8.
    String::from_utf8(out).expect("clean_path only copies valid-UTF-8 input plus ASCII")
}

/// Port of `sanitizedPathJoin` (`cgi.go:326-352`): `filepath.Join(root,
/// filepath.Clean("/"+reqPath))`, traversal-safe by construction because the
/// inner `Clean` call resolves and discards any `..` that would otherwise
/// escape above the root.
pub fn sanitized_path_join(root: &str, req_path: &str) -> String {
    let root = if root.is_empty() { "." } else { root };
    let cleaned_req_path = clean_path(&format!("/{req_path}"));
    let mut joined = clean_path(&format!("{root}/{cleaned_req_path}"));

    if req_path.ends_with('/') && req_path.len() > 1 {
        joined.push('/');
    }

    joined
}

/// Port of `splitCgiPath` (`cgi.go:190-226`), minus the worker branch
/// (`cgi.go:204-215`, `#14`'s `SCRIPT_FILENAME` override is out of scope for
/// this issue). Returns `(doc_uri, path_info, script_name, script_filename)`.
pub fn split_cgi_path(
    path: &str,
    split_path: &[String],
    document_root: &str,
) -> (String, String, String, String) {
    let (mut doc_uri, mut path_info) = (String::new(), String::new());

    let pos = split_pos(path, split_path);
    if pos > -1 {
        let pos = pos as usize;
        doc_uri = path[..pos].to_string();
        path_info = path[pos..].to_string();
    }

    let script_name = ensure_leading_slash(path.strip_suffix(path_info.as_str()).unwrap_or(path));
    let script_filename = sanitized_path_join(document_root, &script_name);

    (doc_uri, path_info, script_name, script_filename)
}

// ---------------------------------------------------------------------------
// Headers -> HTTP_* (cgi.go:150-164, phpheaders.go)
// ---------------------------------------------------------------------------

/// Transcribed verbatim from
/// `vendor/frankenphp/internal/phpheaders/phpheaders.go:15-118`
/// (`CommonRequestHeaders`) -- 101 entries. Go header name -> `$_SERVER` key.
pub const COMMON_HEADERS: &[(&str, &str)] = &[
    ("Accept", "HTTP_ACCEPT"),
    ("Accept-Charset", "HTTP_ACCEPT_CHARSET"),
    ("Accept-Encoding", "HTTP_ACCEPT_ENCODING"),
    ("Accept-Language", "HTTP_ACCEPT_LANGUAGE"),
    (
        "Access-Control-Request-Headers",
        "HTTP_ACCESS_CONTROL_REQUEST_HEADERS",
    ),
    (
        "Access-Control-Request-Method",
        "HTTP_ACCESS_CONTROL_REQUEST_METHOD",
    ),
    ("Authorization", "HTTP_AUTHORIZATION"),
    ("Cache-Control", "HTTP_CACHE_CONTROL"),
    ("Connection", "HTTP_CONNECTION"),
    ("Content-Disposition", "HTTP_CONTENT_DISPOSITION"),
    ("Content-Encoding", "HTTP_CONTENT_ENCODING"),
    ("Content-Length", "HTTP_CONTENT_LENGTH"),
    ("Content-Type", "HTTP_CONTENT_TYPE"),
    ("Cookie", "HTTP_COOKIE"),
    ("Date", "HTTP_DATE"),
    ("Device-Memory", "HTTP_DEVICE_MEMORY"),
    ("Dnt", "HTTP_DNT"),
    ("Downlink", "HTTP_DOWNLINK"),
    ("Dpr", "HTTP_DPR"),
    ("Early-Data", "HTTP_EARLY_DATA"),
    ("Ect", "HTTP_ECT"),
    ("Am-I", "HTTP_AM_I"),
    ("Expect", "HTTP_EXPECT"),
    ("Forwarded", "HTTP_FORWARDED"),
    ("From", "HTTP_FROM"),
    ("Host", "HTTP_HOST"),
    ("If-Match", "HTTP_IF_MATCH"),
    ("If-Modified-Since", "HTTP_IF_MODIFIED_SINCE"),
    ("If-None-Match", "HTTP_IF_NONE_MATCH"),
    ("If-Range", "HTTP_IF_RANGE"),
    ("If-Unmodified-Since", "HTTP_IF_UNMODIFIED_SINCE"),
    ("Keep-Alive", "HTTP_KEEP_ALIVE"),
    ("Max-Forwards", "HTTP_MAX_FORWARDS"),
    ("Origin", "HTTP_ORIGIN"),
    ("Pragma", "HTTP_PRAGMA"),
    ("Proxy-Authorization", "HTTP_PROXY_AUTHORIZATION"),
    ("Range", "HTTP_RANGE"),
    ("Referer", "HTTP_REFERER"),
    ("Rtt", "HTTP_RTT"),
    ("Save-Data", "HTTP_SAVE_DATA"),
    ("Sec-Ch-Ua", "HTTP_SEC_CH_UA"),
    ("Sec-Ch-Ua-Arch", "HTTP_SEC_CH_UA_ARCH"),
    ("Sec-Ch-Ua-Bitness", "HTTP_SEC_CH_UA_BITNESS"),
    ("Sec-Ch-Ua-Full-Version", "HTTP_SEC_CH_UA_FULL_VERSION"),
    (
        "Sec-Ch-Ua-Full-Version-List",
        "HTTP_SEC_CH_UA_FULL_VERSION_LIST",
    ),
    ("Sec-Ch-Ua-Mobile", "HTTP_SEC_CH_UA_MOBILE"),
    ("Sec-Ch-Ua-Model", "HTTP_SEC_CH_UA_MODEL"),
    ("Sec-Ch-Ua-Platform", "HTTP_SEC_CH_UA_PLATFORM"),
    (
        "Sec-Ch-Ua-Platform-Version",
        "HTTP_SEC_CH_UA_PLATFORM_VERSION",
    ),
    ("Sec-Fetch-Dest", "HTTP_SEC_FETCH_DEST"),
    ("Sec-Fetch-Mode", "HTTP_SEC_FETCH_MODE"),
    ("Sec-Fetch-Site", "HTTP_SEC_FETCH_SITE"),
    ("Sec-Fetch-User", "HTTP_SEC_FETCH_USER"),
    ("Sec-Gpc", "HTTP_SEC_GPC"),
    (
        "Service-Worker-Navigation-Preload",
        "HTTP_SERVICE_WORKER_NAVIGATION_PRELOAD",
    ),
    ("Te", "HTTP_TE"),
    ("Priority", "HTTP_PRIORITY"),
    ("Trailer", "HTTP_TRAILER"),
    ("Transfer-Encoding", "HTTP_TRANSFER_ENCODING"),
    ("Upgrade", "HTTP_UPGRADE"),
    (
        "Upgrade-Insecure-Requests",
        "HTTP_UPGRADE_INSECURE_REQUESTS",
    ),
    ("User-Agent", "HTTP_USER_AGENT"),
    ("Via", "HTTP_VIA"),
    ("Viewport-Width", "HTTP_VIEWPORT_WIDTH"),
    ("Want-Digest", "HTTP_WANT_DIGEST"),
    ("Warning", "HTTP_WARNING"),
    ("Width", "HTTP_WIDTH"),
    ("X-Forwarded-For", "HTTP_X_FORWARDED_FOR"),
    ("X-Forwarded-Host", "HTTP_X_FORWARDED_HOST"),
    ("X-Forwarded-Path", "HTTP_X_FORWARDED_PATH"),
    ("X-Forwarded-Prefix", "HTTP_X_FORWARDED_PREFIX"),
    ("X-Forwarded-Proto", "HTTP_X_FORWARDED_PROTO"),
    ("A-Im", "HTTP_A_IM"),
    ("Accept-Datetime", "HTTP_ACCEPT_DATETIME"),
    ("Content-Md5", "HTTP_CONTENT_MD5"),
    ("Http2-Settings", "HTTP_HTTP2_SETTINGS"),
    ("Prefer", "HTTP_PREFER"),
    ("X-Requested-With", "HTTP_X_REQUESTED_WITH"),
    ("Front-End-Https", "HTTP_FRONT_END_HTTPS"),
    ("X-Http-Method-Override", "HTTP_X_HTTP_METHOD_OVERRIDE"),
    ("X-Att-Deviceid", "HTTP_X_ATT_DEVICEID"),
    ("X-Wap-Profile", "HTTP_X_WAP_PROFILE"),
    ("Proxy-Connection", "HTTP_PROXY_CONNECTION"),
    ("X-Uidh", "HTTP_X_UIDH"),
    ("X-Csrf-Token", "HTTP_X_CSRF_TOKEN"),
    ("X-Request-Id", "HTTP_X_REQUEST_ID"),
    ("X-Correlation-Id", "HTTP_X_CORRELATION_ID"),
    ("Cloudflare-Visitor", "HTTP_CLOUDFLARE_VISITOR"),
    (
        "Cloudfront-Viewer-Address",
        "HTTP_CLOUDFRONT_VIEWER_ADDRESS",
    ),
    (
        "Cloudfront-Viewer-Country",
        "HTTP_CLOUDFRONT_VIEWER_COUNTRY",
    ),
    ("X-Amzn-Trace-Id", "HTTP_X_AMZN_TRACE_ID"),
    ("X-Cloud-Trace-Context", "HTTP_X_CLOUD_TRACE_CONTEXT"),
    ("Cf-Ray", "HTTP_CF_RAY"),
    ("Cf-Visitor", "HTTP_CF_VISITOR"),
    ("Cf-Request-Id", "HTTP_CF_REQUEST_ID"),
    ("Cf-Ipcountry", "HTTP_CF_IPCOUNTRY"),
    ("X-Device-Type", "HTTP_X_DEVICE_TYPE"),
    ("X-Network-Info", "HTTP_X_NETWORK_INFO"),
    ("X-Client-Id", "HTTP_X_CLIENT_ID"),
    ("X-Livewire", "HTTP_X_LIVEWIRE"),
    ("X-Real-Ip", "HTTP_X_REAL_IP"),
];

/// Port of `phpheaders.go`'s `loader` (`:125-127`), minus the LRU cache
/// (`otter`, a performance-only detail with no behavioural effect we need to
/// reproduce). NUL-terminated: `frankenphp_register_variable_safe` takes a
/// bare `char *key` (`frankenphp.c:1346`).
fn uncommon_header_key(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + name.len() + 1);
    out.extend_from_slice(b"HTTP_");
    for byte in name.bytes() {
        out.push(if byte == b'-' {
            b'_'
        } else {
            byte.to_ascii_uppercase()
        });
    }
    out.push(0);
    out
}

// ---------------------------------------------------------------------------
// Interned strings (frankenphp.c:1277-1301)
// ---------------------------------------------------------------------------

/// A `zend_string*` from `frankenphp_init_persistent_string`: permanently
/// allocated, `IS_STR_INTERNED`-flagged, never refcounted or freed
/// (`frankenphp.c:1278-1287`; `docs/ARCHITECTURE.md` ownership scheme 4).
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

struct InternedStrings {
    common_headers: HashMap<&'static str, InternedZendString>,
    http_scheme: InternedZendString,
    https_scheme: InternedZendString,
    on: InternedZendString,
    empty: InternedZendString,
}

static INTERNED: OnceLock<InternedStrings> = OnceLock::new();

/// Upstream builds its equivalent (`frankenphp_strings`) once, on the main
/// thread, after boot reaches Ready (`phpmainthread.go:121-125`) -- that
/// ordering lives in `mainthread.rs`, which issue #10 owns and whose spec
/// never mentions this. The in-lane answer for #11 is to build these
/// lazily instead, in a `OnceLock`, on first use. This deviates from
/// upstream's initialisation *ordering* but not its safety properties:
/// `frankenphp_init_persistent_string` is `zend_string_init(...,
/// persistent=1)` (a plain `malloc`) + `zend_string_hash_val` (pure
/// computation) + `GC_ADD_FLAGS(IS_STR_INTERNED)` (a flag write on freshly
/// allocated memory nothing else can see yet) -- none of it touches TSRM,
/// so it has no dependency on which thread, or how early, it runs on.
/// Upstream's own `frankenphp_init_interned_strings` is likewise
/// self-guarding rather than relying on being called exactly once
/// (`frankenphp.c:1290-1293`: `if (frankenphp_strings.remote_addr !=
/// NULL) { return; }`), which is the same "safe to call from more than one
/// place, idempotent in effect" property `OnceLock` gives us directly.
fn interned_strings() -> &'static InternedStrings {
    INTERNED.get_or_init(|| InternedStrings {
        common_headers: COMMON_HEADERS
            .iter()
            .map(|&(name, key)| (name, intern(key)))
            .collect(),
        http_scheme: intern("http"),
        https_scheme: intern("https"),
        on: intern("on"),
        empty: intern(""),
    })
}

fn intern(s: &str) -> InternedZendString {
    // SAFETY: frankenphp_init_persistent_string (frankenphp.c:1278-1287) is
    // a plain persistent malloc + memcpy with no TSRM dependency (see the
    // doc comment on `interned_strings` above), so it is sound to call from
    // any thread at any time, including before `ts_resource()` has run on
    // it. `s.as_ptr()` is valid for `s.len()` bytes for the duration of this
    // call, which is all the C function needs -- it copies the bytes into
    // its own persistent allocation before returning.
    //
    // This is the only call into PHP left anywhere in frankenrust-core's
    // `$_SERVER` path -- everything that registers into `$_SERVER` moved to
    // `frankenrust-sys/shim.c` precisely because it can bail out (see
    // [`ServerVarsBatch`]). This one may stay on the Rust side because it
    // cannot: `persistent = 1` allocates with `__zend_malloc`, which on
    // failure prints "Out of memory" and `exit(1)`s. It never reaches
    // `zend_bailout()`, so there is no `longjmp` to keep off this frame. The
    // request allocator -- the one that does bail out -- is not involved.
    let ptr = unsafe {
        frankenrust_sys::frankenphp_init_persistent_string(s.as_ptr() as *const c_char, s.len())
    };
    InternedZendString(ptr)
}

// ---------------------------------------------------------------------------
// $_SERVER (cgi.go:43-188, frankenphp.c:1220-1268)
// ---------------------------------------------------------------------------

/// Owned buffers for one `frankenphp_register_server_vars` call. Upstream's
/// `addKnownVariablesToServer` (`cgi.go:47-148`) builds the FFI struct and
/// calls the C function in the same expression; splitting buffer
/// computation out like this makes it testable without a live PHP request
/// and without calling the real FFI function, which reads
/// `frankenphp_strings`/`main_thread_env` -- both NULL outside a booted PHP
/// main thread (issue #11's acceptance notes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedServerVars {
    pub remote_addr: Vec<u8>,
    pub remote_host: Vec<u8>,
    pub remote_port: Vec<u8>,
    pub document_root: Vec<u8>,
    pub path_info: Vec<u8>,
    pub php_self: Vec<u8>,
    pub document_uri: Vec<u8>,
    pub script_filename: Vec<u8>,
    pub script_name: Vec<u8>,
    pub server_name: Vec<u8>,
    pub server_port: Vec<u8>,
    pub content_length: Vec<u8>,
    pub server_protocol: Vec<u8>,
    pub http_host: Vec<u8>,
    pub request_uri: Vec<u8>,
    /// Always empty: TLS is out of scope for this port.
    pub ssl_cipher: Vec<u8>,
    pub scheme: Scheme,
    /// `28 + len(request.Header) + len(fc.env) + lengthOfEnv` (`cgi.go:107`)
    /// -- a `zend_hash_extend` sizing hint, not a hard count. `fc.env`
    /// (prepared env) and the OS-env length are both out of scope for this
    /// issue (the latter is #10's `go_init_os_env`), so this is `28 +
    /// len(request.Header)` here; an undercount only costs one rehash.
    pub total_num_vars: usize,
}

/// Port of `addKnownVariablesToServer` (`cgi.go:47-148`), minus the actual
/// FFI call -- see [`ComputedServerVars`]'s doc comment. Returns `None` when
/// there is no request (upstream's `if fc.request != nil` guard,
/// `cgi.go:179`).
pub fn compute_server_vars(ctx: &RequestContext) -> Option<ComputedServerVars> {
    let request = ctx.request.as_ref()?;

    let (ip, port) = split_remote_addr(&request.remote_addr);

    let (mut req_host, mut req_port) = split_host_port(&request.host).unwrap_or_default();
    if req_host.is_empty() {
        req_host = request.host.clone();
    }
    if req_port.is_empty() {
        req_port = match request.scheme {
            Scheme::Https => "443".to_string(),
            Scheme::Http => "80".to_string(),
        };
    }

    // `request.Header.Get("Content-Length")` (cgi.go:93): the raw header
    // text of the *first* Content-Length, not a join of all of them.
    let content_length = request
        .headers
        .get_first("Content-Length")
        .unwrap_or_default()
        .to_vec();
    let php_self = format!("{}{}", ctx.script_name, ctx.path_info);
    let total_num_vars = 28 + request.headers.name_count();

    Some(ComputedServerVars {
        remote_addr: ip.clone().into_bytes(),
        remote_host: ip.into_bytes(),
        remote_port: port.into_bytes(),
        document_root: ctx.document_root.clone().into_bytes(),
        path_info: ctx.path_info.clone().into_bytes(),
        php_self: php_self.into_bytes(),
        document_uri: ctx.doc_uri.clone().into_bytes(),
        script_filename: ctx.script_filename.clone().into_bytes(),
        script_name: ctx.script_name.clone().into_bytes(),
        server_name: req_host.into_bytes(),
        server_port: req_port.into_bytes(),
        content_length,
        server_protocol: request.proto.clone().into_bytes(),
        http_host: request.host.clone().into_bytes(),
        request_uri: ctx.request_uri.clone().into_bytes(),
        ssl_cipher: Vec::new(),
        scheme: request.scheme,
        total_num_vars,
    })
}

impl ComputedServerVars {
    /// Builds the by-value FFI struct `frankenphp_register_server_vars`
    /// consumes (`frankenphp.h:82-119`). Field order below matches the C
    /// declaration exactly -- see `frankenrust-sys/src/layout.rs`'s
    /// compile-time offset assertions, which this struct's layout is
    /// checked against.
    ///
    /// Borrows from `self`'s owned buffers and the process-lifetime interned
    /// strings: `docs/ARCHITECTURE.md` ownership scheme 1 -- the callee
    /// copies every field immediately (`ZVAL_STRINGL_FAST` via
    /// `frankenphp_register_trusted_var`, `frankenphp.c:1200-1218`, or
    /// `ZVAL_STR` for the three interned fields, `frankenphp.c:1257-1262`),
    /// so the returned struct only needs to outlive one call, not the
    /// request.
    fn as_ffi(&self) -> frankenphp_server_vars {
        let interned = interned_strings();
        let (request_scheme, https) = match self.scheme {
            Scheme::Http => (interned.http_scheme, interned.empty),
            Scheme::Https => (interned.https_scheme, interned.on),
        };

        frankenphp_server_vars {
            total_num_vars: self.total_num_vars,
            remote_addr: self.remote_addr.as_ptr() as *mut c_char,
            remote_addr_len: self.remote_addr.len(),
            remote_host: self.remote_host.as_ptr() as *mut c_char,
            remote_host_len: self.remote_host.len(),
            remote_port: self.remote_port.as_ptr() as *mut c_char,
            remote_port_len: self.remote_port.len(),
            document_root: self.document_root.as_ptr() as *mut c_char,
            document_root_len: self.document_root.len(),
            path_info: self.path_info.as_ptr() as *mut c_char,
            path_info_len: self.path_info.len(),
            php_self: self.php_self.as_ptr() as *mut c_char,
            php_self_len: self.php_self.len(),
            document_uri: self.document_uri.as_ptr() as *mut c_char,
            document_uri_len: self.document_uri.len(),
            script_filename: self.script_filename.as_ptr() as *mut c_char,
            script_filename_len: self.script_filename.len(),
            script_name: self.script_name.as_ptr() as *mut c_char,
            script_name_len: self.script_name.len(),
            server_name: self.server_name.as_ptr() as *mut c_char,
            server_name_len: self.server_name.len(),
            server_port: self.server_port.as_ptr() as *mut c_char,
            server_port_len: self.server_port.len(),
            content_length: self.content_length.as_ptr() as *mut c_char,
            content_length_len: self.content_length.len(),
            server_protocol: self.server_protocol.as_ptr() as *mut c_char,
            server_protocol_len: self.server_protocol.len(),
            http_host: self.http_host.as_ptr() as *mut c_char,
            http_host_len: self.http_host.len(),
            request_uri: self.request_uri.as_ptr() as *mut c_char,
            request_uri_len: self.request_uri.len(),
            ssl_cipher: self.ssl_cipher.as_ptr() as *mut c_char,
            ssl_cipher_len: self.ssl_cipher.len(),
            request_scheme: request_scheme.0,
            ssl_protocol: interned.empty.0,
            https: https.0,
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
// already `Send`+`Sync`, see `InternedZendString`). Moving the batch to another
// thread moves `Vec` handles, never the heap bytes they point at, so every
// pointer stays valid across the move. None of the targets has interior
// mutability, and access is serialised anyway: the Rust side only ever reaches
// them under the slot `Mutex`, and the C side only from the PHP thread that
// owns that slot.
unsafe impl Send for HeaderVars {}

/// Owned backing store for one `go_register_server_variables` call: every key
/// and value the C side hands to PHP, plus the descriptor array pointing at
/// them.
///
/// ## Why this is owned by the context and not by a stack frame
///
/// `frankenphp_register_server_vars` / `_known_variable` / `_variable_safe`
/// grow and populate `$_SERVER`'s `HashTable` through the Zend *request*
/// allocator, which on `memory_limit` exhaustion does not return an error --
/// `zend_error_noreturn(E_ERROR, ...)` ends in `zend_bailout()`, a `longjmp`
/// to a `zend_catch` above the callback that skips every frame in between and
/// runs no cleanup. Rust has no defined behaviour for a `longjmp` crossing one
/// of its frames, whatever that frame owns -- and, the trap an earlier
/// revision of this file fell into, that stays true if you catch the bailout
/// in C and re-raise it from Rust: the re-raise jumps *out of* the Rust
/// callback frame just the same.
///
/// So `crates/frankenrust-sys/shim.c` defines `go_register_server_variables`
/// itself and makes the entire registration a pure C frame; Rust only
/// prepares the data and returns before the first Zend call
/// ([`crate::callbacks::servervars::frankenrust_collect_server_vars`]).
///
/// That inverts the ownership question. The buffers C reads cannot live in a
/// Rust frame, because by then there is no Rust frame; they live in the
/// [`RequestContext`] instead, exactly like the [`crate::context::RequestArena`]
/// backing `SG(request_info)`, and are reclaimed at the same point -- the
/// context slot being cleared or replaced, which is upstream's `thread.Unpin()`
/// instant. A bailout partway through the registration therefore leaks
/// nothing: the memory has an owner that the `longjmp` cannot skip past.
pub struct ServerVarsBatch {
    known: ComputedServerVars,
    /// `", "`-joined header values (`cgi.go:153`, `:161`), one per entry of
    /// `headers`, in the same order.
    values: Vec<Vec<u8>>,
    /// NUL-terminated `HTTP_*` keys for the headers that miss the interned
    /// table (`phpheaders.go:126`). Sparse -- only slow-path headers push here.
    keys: Vec<Vec<u8>>,
    headers: HeaderVars,
}

/// Which of `frankenrust_header_var`'s two key forms a header resolved to,
/// recorded during the first pass so that pointers are only taken once
/// `values` and `keys` have stopped growing.
enum HeaderKey {
    /// One of the ~101 pre-interned keys -- the `frankenphp_register_known_variable`
    /// fast path (`cgi.go:152-155`).
    Interned(InternedZendString),
    /// Index into `ServerVarsBatch::keys` -- the `frankenphp_register_variable_safe`
    /// slow path (`cgi.go:158-163`).
    Mangled(usize),
}

/// Port of `go_register_server_variables`' Rust-side half (`cgi.go:174-188`):
/// everything up to, but not including, the calls into PHP.
///
/// `None` when there is no request, matching upstream's `if fc.request != nil`
/// guard (`cgi.go:179`), which gates *both* the known variables and the
/// headers.
pub fn build_server_vars_batch(ctx: &RequestContext) -> Option<ServerVarsBatch> {
    let known = compute_server_vars(ctx)?;
    let request = ctx.request.as_ref()?;
    let interned = interned_strings();

    let mut values: Vec<Vec<u8>> = Vec::with_capacity(request.headers.name_count());
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut key_forms: Vec<HeaderKey> = Vec::with_capacity(request.headers.name_count());

    for (name, header_values) in request.headers.iter() {
        values.push(join_header_values(header_values));
        match interned.common_headers.get(name) {
            Some(interned_key) => key_forms.push(HeaderKey::Interned(*interned_key)),
            None => {
                keys.push(uncommon_header_key(name));
                key_forms.push(HeaderKey::Mangled(keys.len() - 1));
            }
        }
    }

    // Second pass, now that neither `values` nor `keys` will be pushed to
    // again. (Their *inner* buffers would survive further growth of the outer
    // `Vec` anyway -- that is the same address-stability property the arena
    // relies on -- but taking the pointers only once nothing can move is the
    // invariant that is easy to keep true as this function is edited.)
    let headers = key_forms
        .iter()
        .zip(values.iter())
        .map(|(key_form, value)| frankenrust_header_var {
            known_key: match key_form {
                HeaderKey::Interned(key) => key.0,
                HeaderKey::Mangled(_) => std::ptr::null_mut(),
            },
            key: match key_form {
                HeaderKey::Interned(_) => std::ptr::null_mut(),
                HeaderKey::Mangled(index) => keys[*index].as_ptr() as *mut c_char,
            },
            value: value.as_ptr() as *mut c_char,
            value_len: value.len(),
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
    /// the *installed* copy.
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

fn join_header_values(values: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(value);
    }
    out
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
/// `//export` C-ABI wrapper (`callbacks::servervars::go_update_request_info`
/// owns that, per issue #11's module layout). Returns the `Authorization`
/// header value, arena-allocated, or NULL if absent -- `frankenphp.c:358`
/// feeds this straight to `php_handle_auth_data`.
pub fn update_request_info(ctx: &mut RequestContext, info: &mut sapi_request_info) -> *mut c_char {
    let Some(request) = ctx.request.as_ref() else {
        return std::ptr::null_mut();
    };

    let method = request.method.clone();
    let raw_query = request.raw_query.clone();
    let content_length = request.content_length;
    // `request.Header.Get(...)` at cgi.go:306 and :316 -- the first value of
    // each, never a join. A joined Content-Type matches no registered POST
    // reader, and a joined Authorization is un-decodable garbage for
    // `php_handle_auth_data` (frankenphp.c:358).
    let content_type = request
        .headers
        .get_first("Content-Type")
        .map(<[u8]>::to_vec);
    let authorization = request
        .headers
        .get_first("Authorization")
        .map(<[u8]>::to_vec);
    let proto_num = (request.proto_major as i32) * 1000 + request.proto_minor as i32;
    let path_translated = if ctx.path_info.is_empty() {
        None
    } else {
        Some(sanitized_path_join(&ctx.document_root, &ctx.path_info))
    };
    let request_uri = ctx.request_uri.clone();
    // `request` (and so the borrow of `ctx.request`) is not used again past
    // this point, so `ctx.arena` can be borrowed mutably below.

    info.request_method = match cached_method_cstr(&method) {
        Some(cstr) => cstr,
        None => ctx.arena.alloc(method.as_bytes()) as *const c_char,
    };
    info.query_string = ctx.arena.alloc(raw_query.as_bytes());
    info.content_length = content_length as frankenrust_sys::zend_long;

    if let Some(content_type) = content_type {
        // Upstream only sets info.content_type when the header is present
        // (cgi.go:306-308) -- an empty-but-present header is falsy in Go's
        // `if contentType := ...; contentType != ""`, so treat it the same
        // as absent here.
        if !content_type.is_empty() {
            info.content_type = ctx.arena.alloc(&content_type) as *const c_char;
        }
    }

    if let Some(path_translated) = path_translated {
        info.path_translated = ctx.arena.alloc(path_translated.as_bytes());
    }

    info.request_uri = ctx.arena.alloc(request_uri.as_bytes());
    info.proto_num = proto_num;

    match authorization {
        Some(auth) if !auth.is_empty() => ctx.arena.alloc(&auth),
        _ => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Request;
    use std::sync::mpsc;

    fn test_context(request: Option<Request>) -> RequestContext {
        let (tx, _rx) = mpsc::channel();
        RequestContext::new("/var/www".to_string(), None, request, tx)
            .expect("the default split path is valid")
    }

    // --- TestEnsureLeadingSlash (cgi_test.go:10-34) -------------------------

    #[test]
    fn ensure_leading_slash_matches_upstream_table() {
        let cases = [
            ("/index.php", "/index.php"),
            ("index.php", "/index.php"),
            ("/", "/"),
            ("", ""),
            ("/path/to/script.php", "/path/to/script.php"),
            ("path/to/script.php", "/path/to/script.php"),
            ("/index.php/path/info", "/index.php/path/info"),
            ("index.php/path/info", "/index.php/path/info"),
        ];
        for (input, expected) in cases {
            assert_eq!(ensure_leading_slash(input), expected, "input {input:?}");
        }
    }

    // --- TestSplitRemoteAddr (cgi_test.go:36-68) ----------------------------

    #[test]
    fn split_remote_addr_matches_upstream_table() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("ipv4 with port", "1.2.3.4:5", "1.2.3.4", "5"),
            ("ipv6 bracketed with port", "[::1]:443", "::1", "443"),
            (
                "ipv6 zone bracketed with port",
                "[fe80::1%eth0]:443",
                "fe80::1%eth0",
                "443",
            ),
            ("ipv4 without port", "192.168.0.1", "192.168.0.1", ""),
            ("empty", "", "", ""),
            ("only colon", ":", "", ""),
            ("lone open bracket", "[", "[", ""),
            ("open bracket with port", "[:9000", "[", "9000"),
            ("empty brackets", "[]", "", ""),
            ("opening bracket with colon", "[:", "[", ""),
            ("unterminated bracket with port", "[::1:80", "[::1", "80"),
        ];
        for (name, addr, want_ip, want_port) in cases {
            let (ip, port) = split_remote_addr(addr);
            assert_eq!(&ip, want_ip, "{name}: ip for {addr:?}");
            assert_eq!(&port, want_port, "{name}: port for {addr:?}");
        }
    }

    // --- SERVER_NAME derivation (cgi.go:72, distinct from split_remote_addr) --

    #[test]
    fn split_host_port_matches_go_net_split_host_port() {
        assert_eq!(
            split_host_port("example.com:8080"),
            Some(("example.com".to_string(), "8080".to_string()))
        );
        assert_eq!(
            split_host_port("[::1]:80"),
            Some(("::1".to_string(), "80".to_string())),
            "brackets stripped on success"
        );
        assert_eq!(
            split_host_port("[::1]"),
            None,
            "missing port -- caller falls back to request.Host verbatim, brackets kept"
        );
    }

    // --- TestSplitPos (cgi_test.go:70-280), including the GHSA regressions ---

    #[test]
    fn split_pos_matches_upstream_table() {
        let php = vec![".php".to_string()];
        let php_phtml = vec![".php".to_string(), ".phtml".to_string()];

        let cases: &[(&str, &[String], isize)] = &[
            ("/path/to/script.php", &php, 19),
            ("/path/to/script.php/some/path", &php, 19),
            ("/path/to/script.PHP", &php, 19),
            ("/path/to/script.PhP/info", &php, 19),
            ("/path/to/script.txt", &php, -1),
            ("/path/to/script.php", &[], 0),
            ("/path/to/script.php", &php_phtml, 19),
            ("/path/to/script.phtml", &php_phtml, 21),
            ("/ȺȺȺȺshell.php", &php, 18),
            ("/ȺȺȺȺshell.php/path/info", &php, 18),
            ("/ȺȺȺȺshell.php.txt.php", &php, 18),
            ("/ȺȺȺȺshell.PHP", &php, 18),
            ("/path/Ⱥtest/script.php", &php, 23),
            ("/Ⱥ/script.php", &php, 14),
            ("/İtest.php", &php, 11),
            ("/PATH/TO/SCRIPT.PHP/INFO", &php, 19),
            ("/index.php", &php, 10),
            ("/test.php.bak", &php, 9),
            ("/PoC-match-unset.¡.txt", &php, -1),
            ("/script.p\u{a1}p", &php, -1),
            ("/shell\u{fe52}php", &php, -1),
            ("/shell\u{ff0e}php", &php, -1),
            ("/shell.\u{ff50}hp", &php, -1),
            ("/shell.\u{24df}\u{24d7}\u{24df}", &php, -1),
            ("/shell.\u{1d5fd}\u{1d5f5}\u{1d5fd}", &php, -1),
            ("/shell.\u{1d4c5}\u{1d4bd}\u{1d4c5}", &php, -1),
            (
                "/shell.\u{24df}\u{24d7}\u{24df}.anything-after-payload.php",
                &php,
                43,
            ),
        ];

        for (path, split_path, want) in cases {
            assert_eq!(split_pos(path, split_path), *want, "path {path:?}");
        }
    }

    // --- TestSplitPosUnicodeSecurityRegression (cgi_test.go:285-305) ---------

    #[test]
    fn split_pos_unicode_case_folding_length_expansion_regression() {
        // U+023A (Ⱥ, 2 bytes) lowercases to U+2C65 (ⱥ, 3 bytes); the correct
        // implementation never lowercases into a new buffer, so the position
        // is computed directly against the original bytes.
        let path = "/ȺȺȺȺshell.php.txt.php";
        let split = vec![".php".to_string()];
        assert_eq!(split_pos(path, &split), 18);
    }

    // --- TestSplitPosSecurityRegressionUnicodeBypass (cgi_test.go:312-332) ---

    #[test]
    fn split_pos_security_regression_unicode_bypass() {
        let split = vec![".php".to_string()];
        let payloads: &[&str] = &[
            "/PoC-match-unset.\u{a1}.txt",
            "/shell\u{fe52}php",
            "/shell\u{ff0e}php",
            "/shell.\u{ff50}hp",
            "/shell.p\u{ff48}p",
            "/shell.ph\u{ff50}",
            "/shell.\u{1d5c1}\u{1d5b5}\u{1d5c1}",
            "/shell.\u{1d5fd}\u{1d5f5}\u{1d5fd}",
            "/shell.\u{1d4c5}\u{1d4bd}\u{1d4c5}",
            "/shell.\u{24df}\u{24d7}\u{24df}",
        ];
        for payload in payloads {
            assert_eq!(
                split_pos(payload, &split),
                -1,
                "payload {payload:?} must not match"
            );
        }
    }

    // --- Fresh tests for sanitizedPathJoin (cgi.go:326-352) ------------------

    #[test]
    fn sanitized_path_join_rejects_dotdot_traversal() {
        assert_eq!(
            sanitized_path_join("/var/www", "../../etc/passwd"),
            "/var/www/etc/passwd"
        );
        assert_eq!(
            sanitized_path_join("/var/www", "/../../../etc/passwd"),
            "/var/www/etc/passwd"
        );
        assert_eq!(sanitized_path_join("/var/www", "/a/../../b"), "/var/www/b");
    }

    #[test]
    fn sanitized_path_join_handles_absolute_request_paths() {
        assert_eq!(
            sanitized_path_join("/var/www", "/index.php"),
            "/var/www/index.php"
        );
        assert_eq!(
            sanitized_path_join("/var/www", "index.php"),
            "/var/www/index.php"
        );
        assert_eq!(sanitized_path_join("/var/www", "/"), "/var/www");
    }

    #[test]
    fn sanitized_path_join_does_not_decode_percent_encoded_separators() {
        // "%2f"/"%2e" are not '/'/'.' bytes -- this function receives an
        // already-decoded path from its caller and must not itself treat a
        // literal percent-sequence as a traversal attempt or a separator.
        let joined = sanitized_path_join("/var/www", "/foo%2f..%2f..%2fetc%2fpasswd");
        assert_eq!(joined, "/var/www/foo%2f..%2f..%2fetc%2fpasswd");
        assert!(joined.starts_with("/var/www/"));
    }

    #[test]
    fn sanitized_path_join_preserves_trailing_slash() {
        assert_eq!(sanitized_path_join("/var/www", "/dir/"), "/var/www/dir/");
    }

    #[test]
    fn sanitized_path_join_defaults_empty_root_to_dot() {
        assert_eq!(sanitized_path_join("", "/etc/passwd"), "etc/passwd");
    }

    #[test]
    fn sanitized_path_join_resolves_dotdot_in_the_document_root() {
        // Regression test for a one-byte mis-port of filepath.Clean's `..`
        // backtrack: it stopped on the last byte still present rather than on
        // the separator one past it, leaving the separator behind. The outer
        // Clean collapses the resulting "//" almost everywhere, which is why
        // only a `..` inside the *root* shows it.
        assert_eq!(
            sanitized_path_join("/var/www/..", "/index.php"),
            "/var/index.php"
        );
        assert_eq!(
            sanitized_path_join("/var/www/../www", "/a.php"),
            "/var/www/a.php"
        );
    }

    #[test]
    fn clean_path_matches_go_filepath_clean() {
        // Rows lifted from Go's path/filepath TestClean, restricted to the
        // Unix separator (the only one build.rs supports).
        let cases = [
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("../../", "../.."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("///abc", "/abc"),
            ("//abc//", "/abc"),
            ("abc//", "abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            // The rows the off-by-one got wrong: a `..` that lands the write
            // cursor on a separator inside the result.
            ("/var/www/..", "/var"),
            ("/a/b/..", "/a"),
            ("a/b/..", "a"),
            ("/a/b/../c", "/a/c"),
        ];
        for (input, expected) in cases {
            assert_eq!(clean_path(input), expected, "clean_path({input:?})");
        }
    }

    // --- WithRequestSplitPath (requestoptions.go:86-113) ---------------------

    #[test]
    fn normalize_split_path_matches_upstream_table() {
        // Ported from requestoptions_test.go:20-52.
        assert_eq!(
            normalize_split_path(vec![".php".to_string()]).unwrap(),
            vec![".php".to_string()]
        );
        assert_eq!(
            normalize_split_path(vec![".PHP".to_string()]).unwrap(),
            vec![".php".to_string()]
        );
        assert_eq!(
            normalize_split_path(vec![".PhP".to_string(), ".PHTML".to_string()]).unwrap(),
            vec![".php".to_string(), ".phtml".to_string()]
        );
        assert_eq!(
            normalize_split_path(Vec::new()).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            normalize_split_path(vec![".php".to_string(), ".Ⱥphp".to_string()]),
            Err(InvalidSplitPath)
        );
        assert_eq!(
            normalize_split_path(vec![".phpⱥ".to_string()]),
            Err(InvalidSplitPath)
        );
    }

    #[test]
    fn split_pos_matches_a_normalised_uppercase_split_path() {
        // The reason normalisation is load-bearing: split_pos folds the bytes
        // of the *path* only, so it compares against the split entry
        // verbatim and ".PHP" would never match anything.
        let raw = vec![".PHP".to_string()];
        assert_eq!(split_pos("/index.php", &raw), -1);

        let normalised = normalize_split_path(raw).unwrap();
        assert_eq!(split_pos("/index.php", &normalised), 10);
    }

    // --- Headers -> HTTP_* (issue #11 acceptance) -----------------------------

    #[test]
    fn uncommon_header_key_maps_accept_encoding() {
        let key = uncommon_header_key("Accept-Encoding");
        assert_eq!(key, b"HTTP_ACCEPT_ENCODING\0");
    }

    #[test]
    fn uncommon_header_key_agrees_with_common_headers_table_for_every_entry() {
        for &(name, php_key) in COMMON_HEADERS {
            let mut mangled = uncommon_header_key(name);
            assert_eq!(mangled.pop(), Some(0), "must be NUL-terminated: {name}");
            assert_eq!(
                String::from_utf8(mangled).unwrap(),
                php_key,
                "generic mangler disagrees with the pre-interned table for {name}"
            );
        }
    }

    // --- frankenphp_server_vars field values (issue #11 acceptance) ----------

    fn request_for_server_vars() -> Request {
        Request::new("GET", "/index.php", "a=1").with_header("Content-Length", b"123".to_vec())
    }

    #[test]
    fn server_vars_http_host_keeps_port_server_name_does_not() {
        let mut request = request_for_server_vars();
        request.host = "example.com:8080".to_string();
        let ctx = test_context(Some(request));

        let vars = compute_server_vars(&ctx).unwrap();
        assert_eq!(vars.http_host, b"example.com:8080");
        assert_eq!(vars.server_name, b"example.com");
    }

    #[test]
    fn server_vars_server_name_strips_ipv6_brackets_on_success() {
        let mut request = request_for_server_vars();
        request.host = "[::1]:80".to_string();
        let ctx = test_context(Some(request));

        let vars = compute_server_vars(&ctx).unwrap();
        assert_eq!(vars.http_host, b"[::1]:80");
        assert_eq!(vars.server_name, b"::1");
    }

    #[test]
    fn server_vars_server_name_keeps_ipv6_brackets_without_port() {
        let mut request = request_for_server_vars();
        request.host = "[::1]".to_string();
        let ctx = test_context(Some(request));

        let vars = compute_server_vars(&ctx).unwrap();
        assert_eq!(vars.http_host, b"[::1]");
        assert_eq!(
            vars.server_name, b"[::1]",
            "SplitHostPort errors on a portless bracketed host, so upstream falls back to \
             request.Host verbatim -- brackets kept, unlike the success case"
        );
    }

    #[test]
    fn server_vars_content_length_is_raw_header_and_empty_when_absent() {
        let mut with_header = request_for_server_vars();
        with_header.host = "example.com".to_string();
        let ctx = test_context(Some(with_header));
        assert_eq!(compute_server_vars(&ctx).unwrap().content_length, b"123");

        let mut without_header = Request::new("GET", "/index.php", "");
        without_header.host = "example.com".to_string();
        let ctx = test_context(Some(without_header));
        assert_eq!(compute_server_vars(&ctx).unwrap().content_length, b"");
    }

    #[test]
    fn server_vars_php_self_is_script_name_plus_path_info() {
        let mut request = Request::new("GET", "/index.php/extra/path", "");
        request.host = "example.com".to_string();
        let ctx = test_context(Some(request));

        assert_eq!(ctx.script_name, "/index.php");
        assert_eq!(ctx.path_info, "/extra/path");

        let vars = compute_server_vars(&ctx).unwrap();
        assert_eq!(vars.php_self, b"/index.php/extra/path");
    }

    #[test]
    fn server_vars_server_port_falls_back_by_scheme() {
        let mut http_request = Request::new("GET", "/index.php", "");
        http_request.host = "example.com".to_string();
        http_request.scheme = Scheme::Http;
        let ctx = test_context(Some(http_request));
        assert_eq!(compute_server_vars(&ctx).unwrap().server_port, b"80");

        let mut https_request = Request::new("GET", "/index.php", "");
        https_request.host = "example.com".to_string();
        https_request.scheme = Scheme::Https;
        let ctx = test_context(Some(https_request));
        assert_eq!(compute_server_vars(&ctx).unwrap().server_port, b"443");

        let mut explicit_port_request = Request::new("GET", "/index.php", "");
        explicit_port_request.host = "example.com:9000".to_string();
        let ctx = test_context(Some(explicit_port_request));
        assert_eq!(
            compute_server_vars(&ctx).unwrap().server_port,
            b"9000",
            "an explicit port must win over the scheme fallback"
        );
    }

    #[test]
    fn server_vars_content_length_is_the_first_header_not_a_join() {
        // request.Header.Get (cgi.go:93) returns v[0].
        let mut request = Request::new("GET", "/index.php", "")
            .with_header("Content-Length", b"5".to_vec())
            .with_header("Content-Length", b"9".to_vec());
        request.host = "example.com".to_string();
        let ctx = test_context(Some(request));

        assert_eq!(compute_server_vars(&ctx).unwrap().content_length, b"5");
    }

    #[test]
    fn compute_server_vars_is_none_without_a_request() {
        let ctx = test_context(None);
        assert!(compute_server_vars(&ctx).is_none());
        assert!(build_server_vars_batch(&ctx).is_none());
    }

    // --- the filled frankenphp_server_vars (issue #11 acceptance) ------------
    //
    // `as_ffi()` is the field mapping itself -- 36 fields, and a transposed
    // pair corrupts $_SERVER silently rather than crashing. The compile-time
    // offset assertions in frankenrust-sys/src/layout.rs:93-154 pin where each
    // field *is*; only a test on a filled struct can pin what goes *in* it.
    //
    // Deliberately does not call frankenphp_register_server_vars: that reads
    // frankenphp_strings for its keys and copies main_thread_env
    // (frankenphp.c:1223-1229), both NULL without a booted PHP main thread, so
    // calling it here would segfault rather than fail. `as_ffi()` itself only
    // needs frankenphp_init_persistent_string, which is a plain persistent
    // malloc with no TSRM dependency -- see `interned_strings`.

    /// Reads one of the FFI struct's `char *`/`size_t` pairs back as bytes.
    ///
    /// # Safety
    /// `ptr` must be the pointer half of a pair whose backing buffer is still
    /// alive -- i.e. the `ComputedServerVars` that `as_ffi()` borrowed from.
    unsafe fn ffi_field(ptr: *mut c_char, len: usize) -> Vec<u8> {
        assert!(!ptr.is_null(), "as_ffi() must never hand C a NULL pointer");
        // SAFETY: forwarded from this helper's own contract; every caller
        // below keeps the source `ComputedServerVars` alive across the read.
        unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }.to_vec()
    }

    #[test]
    fn as_ffi_puts_every_value_in_the_right_field() {
        let mut request = Request::new("GET", "/index.php/extra", "x=1")
            .with_header("Content-Length", b"7".to_vec());
        request.host = "example.com:8080".to_string();
        request.remote_addr = "1.2.3.4:5678".to_string();
        request.proto = "HTTP/1.1".to_string();
        let ctx = test_context(Some(request));

        let vars = compute_server_vars(&ctx).unwrap();
        let ffi = vars.as_ffi();

        // SAFETY: `vars` outlives every read below, and `ffi` borrows from it.
        unsafe {
            assert_eq!(ffi_field(ffi.remote_addr, ffi.remote_addr_len), b"1.2.3.4");
            assert_eq!(ffi_field(ffi.remote_host, ffi.remote_host_len), b"1.2.3.4");
            assert_eq!(ffi_field(ffi.remote_port, ffi.remote_port_len), b"5678");
            assert_eq!(
                ffi_field(ffi.document_root, ffi.document_root_len),
                b"/var/www"
            );
            assert_eq!(ffi_field(ffi.path_info, ffi.path_info_len), b"/extra");
            assert_eq!(
                ffi_field(ffi.php_self, ffi.php_self_len),
                b"/index.php/extra",
                "PHP_SELF is script_name + path_info (cgi.go:102)"
            );
            assert_eq!(
                ffi_field(ffi.document_uri, ffi.document_uri_len),
                b"/index.php"
            );
            assert_eq!(
                ffi_field(ffi.script_filename, ffi.script_filename_len),
                b"/var/www/index.php"
            );
            assert_eq!(
                ffi_field(ffi.script_name, ffi.script_name_len),
                b"/index.php"
            );
            assert_eq!(
                ffi_field(ffi.server_name, ffi.server_name_len),
                b"example.com",
                "SERVER_NAME is SplitHostPort's host (cgi.go:71-76)"
            );
            assert_eq!(ffi_field(ffi.server_port, ffi.server_port_len), b"8080");
            assert_eq!(
                ffi_field(ffi.content_length, ffi.content_length_len),
                b"7",
                "CONTENT_LENGTH is the raw header (cgi.go:93)"
            );
            assert_eq!(
                ffi_field(ffi.server_protocol, ffi.server_protocol_len),
                b"HTTP/1.1"
            );
            assert_eq!(
                ffi_field(ffi.http_host, ffi.http_host_len),
                b"example.com:8080",
                "HTTP_HOST keeps the port (cgi.go:136)"
            );
            assert_eq!(
                ffi_field(ffi.request_uri, ffi.request_uri_len),
                b"/index.php/extra?x=1"
            );
            assert_eq!(
                ffi_field(ffi.ssl_cipher, ffi.ssl_cipher_len),
                b"",
                "TLS is out of scope: ssl_cipher is always empty here"
            );
        }
        assert_eq!(ffi.total_num_vars, vars.total_num_vars);

        // The three zend_string* fields are minted, not read from the C global
        // frankenphp_strings (issue #11): http scheme, no HTTPS, no TLS.
        let interned = interned_strings();
        assert_eq!(ffi.request_scheme, interned.http_scheme.0);
        assert_eq!(ffi.https, interned.empty.0);
        assert_eq!(ffi.ssl_protocol, interned.empty.0);
    }

    #[test]
    fn as_ffi_reports_https_and_the_443_port_fallback() {
        let mut request = Request::new("GET", "/index.php", "");
        request.host = "example.com".to_string();
        request.scheme = Scheme::Https;
        let ctx = test_context(Some(request));

        let vars = compute_server_vars(&ctx).unwrap();
        let ffi = vars.as_ffi();

        // SAFETY: `vars` outlives the reads; `ffi` borrows from it.
        unsafe {
            assert_eq!(
                ffi_field(ffi.server_port, ffi.server_port_len),
                b"443",
                "SERVER_PORT falls back by scheme, RFC 3875 4.1.15 (cgi.go:79-92)"
            );
            assert_eq!(
                ffi_field(ffi.content_length, ffi.content_length_len),
                b"",
                "CONTENT_LENGTH is empty, not absent, when the header is missing"
            );
        }

        let interned = interned_strings();
        assert_eq!(ffi.request_scheme, interned.https_scheme.0);
        assert_eq!(ffi.https, interned.on.0, "$_SERVER['HTTPS'] is \"on\"");
        assert_eq!(
            ffi.ssl_protocol, interned.empty.0,
            "TLS is out of scope: ssl_protocol is always empty here"
        );
    }

    #[test]
    fn as_ffi_server_name_keeps_the_ipv6_bracket_asymmetry() {
        // net.SplitHostPort strips brackets on success and errors when there
        // is no port, in which case upstream assigns request.Host verbatim --
        // brackets kept (cgi.go:71-76).
        for (host, want_server_name, want_port) in [
            ("example.com:8080", "example.com", "8080"),
            ("[::1]:80", "::1", "80"),
            ("[::1]", "[::1]", "80"),
        ] {
            let mut request = Request::new("GET", "/index.php", "");
            request.host = host.to_string();
            let ctx = test_context(Some(request));

            let vars = compute_server_vars(&ctx).unwrap();
            let ffi = vars.as_ffi();
            // SAFETY: `vars` outlives the reads; `ffi` borrows from it.
            unsafe {
                assert_eq!(
                    ffi_field(ffi.server_name, ffi.server_name_len),
                    want_server_name.as_bytes(),
                    "SERVER_NAME for Host: {host}"
                );
                assert_eq!(
                    ffi_field(ffi.http_host, ffi.http_host_len),
                    host.as_bytes(),
                    "HTTP_HOST is the Host header verbatim"
                );
                assert_eq!(
                    ffi_field(ffi.server_port, ffi.server_port_len),
                    want_port.as_bytes(),
                    "SERVER_PORT for Host: {host}"
                );
            }
        }
    }

    // --- the header batch handed to shim.c -----------------------------------

    /// Reads one descriptor of the C batch back into `(key, value)`, where the
    /// key is either the interned `zend_string*` (fast path) or the mangled
    /// NUL-terminated bytes (slow path).
    ///
    /// # Safety
    /// `batch` must borrow from a `ServerVarsBatch` that is still alive.
    unsafe fn read_header(
        batch: &frankenrust_server_vars_batch,
        index: usize,
    ) -> (Option<*mut zend_string>, Option<Vec<u8>>, Vec<u8>) {
        assert!(index < batch.num_headers);
        // SAFETY: `headers` points at `num_headers` initialised descriptors
        // owned by the live `ServerVarsBatch` this batch borrows from.
        let header = unsafe { &*batch.headers.add(index) };
        let value =
            // SAFETY: same -- `value` points into that batch's `values`.
            unsafe { std::slice::from_raw_parts(header.value as *const u8, header.value_len) }
                .to_vec();

        if header.known_key.is_null() {
            assert!(!header.key.is_null(), "slow path needs a mangled key");
            // SAFETY: `uncommon_header_key` NUL-terminates, so CStr can find
            // the end inside the batch-owned buffer.
            let key = unsafe { std::ffi::CStr::from_ptr(header.key) }
                .to_bytes()
                .to_vec();
            (None, Some(key), value)
        } else {
            assert!(
                header.key.is_null(),
                "fast path must not also set the mangled key"
            );
            (Some(header.known_key), None, value)
        }
    }

    #[test]
    fn build_server_vars_batch_joins_multi_valued_headers_and_picks_the_right_key_path() {
        let mut request = Request::new("GET", "/index.php", "")
            // Not in phpheaders.go -> frankenphp_register_variable_safe.
            .with_header("X-Foo", b"a".to_vec())
            .with_header("X-Foo", b"b".to_vec())
            // In phpheaders.go -> frankenphp_register_known_variable.
            .with_header("Accept-Encoding", b"gzip".to_vec());
        request.host = "example.com".to_string();
        let ctx = test_context(Some(request));

        let batch = build_server_vars_batch(&ctx).unwrap();
        let c_batch = batch.as_c_batch();
        assert_eq!(c_batch.num_headers, 2);

        // SAFETY: `batch` outlives every read; `c_batch` borrows from it.
        unsafe {
            let (known, mangled, value) = read_header(&c_batch, 0);
            assert!(known.is_none(), "X-Foo is not a pre-interned key");
            assert_eq!(mangled.unwrap(), b"HTTP_X_FOO");
            assert_eq!(value, b"a, b", "multi-valued headers join with \", \"");

            let (known, mangled, value) = read_header(&c_batch, 1);
            assert_eq!(
                known,
                Some(interned_strings().common_headers["Accept-Encoding"].0),
                "Accept-Encoding must take the pre-interned fast path"
            );
            assert!(mangled.is_none());
            assert_eq!(value, b"gzip");
        }
    }

    #[test]
    fn the_c_batch_borrows_from_the_context_not_from_the_collecting_frame() {
        // The property the whole shim.c split rests on: by the time C reads
        // these pointers, the Rust frame that produced them is gone, so they
        // must target memory the RequestContext owns. Built here the same way
        // `frankenrust_collect_server_vars` does -- install first, then take
        // the C view of the *installed* copy -- and read back only after the
        // building expression has returned.
        let mut request =
            Request::new("GET", "/index.php", "").with_header("X-Foo", b"bar".to_vec());
        request.host = "example.com".to_string();
        let mut ctx = test_context(Some(request));

        let c_batch = {
            let batch = build_server_vars_batch(&ctx).unwrap();
            ctx.install_server_vars(batch)
        };

        // SAFETY: `ctx` is still alive and owns the installed batch, so every
        // pointer in `c_batch` is valid -- which is exactly what this test
        // exists to demonstrate, the local `batch` above having been moved
        // into the context and its building frame long gone.
        unsafe {
            assert_eq!(
                ffi_field(c_batch.vars.http_host, c_batch.vars.http_host_len),
                b"example.com"
            );
            let (_, mangled, value) = read_header(&c_batch, 0);
            assert_eq!(mangled.unwrap(), b"HTTP_X_FOO");
            assert_eq!(value, b"bar");
        }
    }

    // --- sapi_request_info population (issue #11 acceptance) -----------------

    fn default_request_info() -> sapi_request_info {
        // SAFETY: sapi_request_info is a plain-old-data struct (build.rs
        // enables `derive_default` for bindgen output); zeroing it does not
        // require a live Zend engine, we never dereference any of its
        // pointer fields here.
        sapi_request_info::default()
    }

    #[test]
    fn update_request_info_passes_content_length_verbatim_including_unknown() {
        let mut request = Request::new("GET", "/index.php", "");
        request.content_length = -1;
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        assert_eq!(info.content_length, -1);
    }

    #[test]
    fn update_request_info_leaves_content_type_null_when_absent() {
        let request = Request::new("GET", "/index.php", "");
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        assert!(info.content_type.is_null());
    }

    #[test]
    fn update_request_info_sets_content_type_when_present() {
        let request = Request::new("GET", "/index.php", "")
            .with_header("Content-Type", b"text/plain".to_vec());
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        assert!(!info.content_type.is_null());
        // SAFETY: the pointer was just filled by update_request_info from
        // ctx.arena, which `ctx` (and so the arena) still owns here.
        let read_back = unsafe { std::ffi::CStr::from_ptr(info.content_type) };
        assert_eq!(read_back.to_bytes(), b"text/plain");
    }

    #[test]
    fn update_request_info_uses_the_first_content_type_when_duplicated() {
        // Regression test: joining two Content-Type headers produces
        // "text/plain, application/json", which matches no registered POST
        // reader, so the body silently falls through to the default handler.
        let request = Request::new("POST", "/index.php", "")
            .with_header("Content-Type", b"text/plain".to_vec())
            .with_header("Content-Type", b"application/json".to_vec());
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        // SAFETY: the pointer was just filled by update_request_info from
        // ctx.arena, which `ctx` (and so the arena) still owns here.
        let read_back = unsafe { std::ffi::CStr::from_ptr(info.content_type) };
        assert_eq!(read_back.to_bytes(), b"text/plain");
    }

    #[test]
    fn update_request_info_returns_the_first_authorization_when_duplicated() {
        // Regression test: a joined Authorization reaches php_handle_auth_data
        // (frankenphp.c:358) as un-decodable garbage instead of the first
        // credential.
        let request = Request::new("GET", "/index.php", "")
            .with_header("Authorization", b"Basic dXNlcjpwYXNz".to_vec())
            .with_header("Authorization", b"Basic b3RoZXI6b3RoZXI=".to_vec());
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        let auth = update_request_info(&mut ctx, &mut info);
        assert!(!auth.is_null());
        // SAFETY: as above -- arena-owned, and `ctx` is still alive.
        let read_back = unsafe { std::ffi::CStr::from_ptr(auth) };
        assert_eq!(read_back.to_bytes(), b"Basic dXNlcjpwYXNz");
    }

    #[test]
    fn update_request_info_computes_proto_num() {
        let mut request = Request::new("GET", "/index.php", "");
        request.proto_major = 1;
        request.proto_minor = 1;
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        assert_eq!(info.proto_num, 1001);
    }

    #[test]
    fn update_request_info_returns_null_without_authorization_header() {
        let request = Request::new("GET", "/index.php", "");
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        let auth = update_request_info(&mut ctx, &mut info);
        assert!(auth.is_null());
    }

    #[test]
    fn update_request_info_returns_authorization_header_value() {
        let request = Request::new("GET", "/index.php", "")
            .with_header("Authorization", b"Bearer xyz".to_vec());
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        let auth = update_request_info(&mut ctx, &mut info);
        assert!(!auth.is_null());
        // SAFETY: same reasoning as update_request_info_sets_content_type_when_present.
        let read_back = unsafe { std::ffi::CStr::from_ptr(auth) };
        assert_eq!(read_back.to_bytes(), b"Bearer xyz");
    }

    #[test]
    fn update_request_info_uses_cached_method_constants() {
        let request = Request::new("POST", "/index.php", "");
        let mut ctx = test_context(Some(request));
        let mut info = default_request_info();

        update_request_info(&mut ctx, &mut info);
        assert!(!info.request_method.is_null());
        // SAFETY: cached_method_cstr's pointer is 'static (a C-string
        // literal), always valid to read regardless of ctx's lifetime.
        let read_back = unsafe { std::ffi::CStr::from_ptr(info.request_method) };
        assert_eq!(read_back.to_bytes(), b"POST");
    }
}
