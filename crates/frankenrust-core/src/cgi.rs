//! Pure CGI path, address, and HTTP-header helpers.
//!
//! This is the byte-oriented part of `vendor/frankenphp/cgi.go`. It stays
//! independent of request contexts and PHP's FFI so callers can derive and
//! validate values before entering a PHP thread.

use std::fmt;

/// Port of `ensureLeadingSlash` (`cgi.go:378-384`).
pub fn ensure_leading_slash(path: &[u8]) -> Vec<u8> {
    if path.is_empty() || path.first() == Some(&b'/') {
        path.to_vec()
    } else {
        let mut result = Vec::with_capacity(path.len() + 1);
        result.push(b'/');
        result.extend_from_slice(path);
        result
    }
}

/// The `ErrInvalidSplitPath` condition from `requestoptions.go`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSplitPath;

impl fmt::Display for InvalidSplitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("split path contains non-ASCII characters")
    }
}

impl std::error::Error for InvalidSplitPath {}

/// Validated, ASCII-lowercase CGI split strings.
///
/// Upstream performs this validation in `WithRequestSplitPath`
/// (`requestoptions.go:86-102`) and `splitPos` relies on it: only the request
/// path is folded while matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPath {
    entries: Vec<Vec<u8>>,
}

impl SplitPath {
    /// Rejects non-ASCII entries and lowercases ASCII entries, as upstream
    /// does when applying the request option.
    pub fn new<I, B>(entries: I) -> Result<Self, InvalidSplitPath>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        Ok(Self {
            entries: normalize_split_path(entries)?,
        })
    }

    /// Returns the normalized entries used by [`split_pos`].
    pub fn as_slice(&self) -> &[Vec<u8>] {
        &self.entries
    }

    /// Distinguishes an explicitly empty split list from an absent one.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Validates and normalizes split strings without wrapping them in
/// [`SplitPath`].
pub fn normalize_split_path<I, B>(entries: I) -> Result<Vec<Vec<u8>>, InvalidSplitPath>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    entries
        .into_iter()
        .map(|entry| {
            let entry = entry.as_ref();
            if !entry.is_ascii() {
                return Err(InvalidSplitPath);
            }
            Ok(entry.iter().map(u8::to_ascii_lowercase).collect())
        })
        .collect()
}

/// Port of `splitPos` (`cgi.go:238-279`).
///
/// Matching is ASCII-only and case-insensitive on the `path` side. A byte at
/// or above `0x80` can never match, which is the security property introduced
/// for GHSA-3g8v-8r37-cgjm and GHSA-v4h7-cj44-8fc8. Entries are expected to
/// have passed through [`SplitPath::new`] or [`normalize_split_path`].
pub fn split_pos<S>(path: &[u8], split_path: &[S]) -> isize
where
    S: AsRef<[u8]>,
{
    if split_path.is_empty() {
        return 0;
    }

    for split in split_path {
        let split = split.as_ref();
        if split.is_empty() || split.len() > path.len() {
            continue;
        }

        for start in 0..=(path.len() - split.len()) {
            let matched = path[start..start + split.len()].iter().zip(split).all(
                |(&path_byte, &split_byte)| {
                    path_byte < 0x80 && path_byte.to_ascii_lowercase() == split_byte
                },
            );

            if matched {
                return (start + split.len()) as isize;
            }
        }
    }

    -1
}

/// The four path values computed by `splitCgiPath` (`cgi.go:191-230`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgiPaths {
    pub doc_uri: Vec<u8>,
    pub path_info: Vec<u8>,
    pub script_name: Vec<u8>,
    pub script_filename: Vec<u8>,
}

/// Splits a request path and derives its CGI path variables.
///
/// `None` has upstream's default `.php` behavior. `Some` with an empty
/// [`SplitPath`] is intentionally different: `DOCUMENT_URI` becomes empty and
/// `PATH_INFO` becomes the whole request path. The worker override from
/// `cgi.go:203-215` is handled by the worker layer, not this pure function.
pub fn split_cgi_path(
    document_root: &[u8],
    split_path: Option<&SplitPath>,
    path: &[u8],
) -> CgiPaths {
    let default_split = [b".php".as_slice()];
    let position = match split_path {
        Some(split_path) => split_pos(path, split_path.as_slice()),
        None => split_pos(path, &default_split),
    };

    let (doc_uri, path_info) = if position >= 0 {
        let position = position as usize;
        (path[..position].to_vec(), path[position..].to_vec())
    } else {
        (Vec::new(), Vec::new())
    };

    // `path_info` is always a suffix selected from `path` above. Keeping a
    // defensive fallback makes this helper non-panicking if that invariant is
    // changed during a later refactor.
    let script_path = path.strip_suffix(path_info.as_slice()).unwrap_or(path);
    let script_name = ensure_leading_slash(script_path);
    let script_filename = sanitized_path_join(document_root, &script_name);

    CgiPaths {
        doc_uri,
        path_info,
        script_name,
        script_filename,
    }
}

/// Port of Go's Unix `path/filepath.Clean` for arbitrary bytes.
///
/// FrankenRust's PHP build targets Unix, where `/` is the only separator.
fn clean_path(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }

    let rooted = path.first() == Some(&b'/');
    let mut output = Vec::with_capacity(path.len());
    let mut read = usize::from(rooted);
    let mut dotdot = usize::from(rooted);

    if rooted {
        output.push(b'/');
    }

    while read < path.len() {
        if path[read] == b'/'
            || (path[read] == b'.' && (read + 1 == path.len() || path[read + 1] == b'/'))
        {
            read += 1;
        } else if path[read] == b'.'
            && read + 1 < path.len()
            && path[read + 1] == b'.'
            && (read + 2 == path.len() || path[read + 2] == b'/')
        {
            read += 2;
            if output.len() > dotdot {
                let mut write = output.len() - 1;
                while write > dotdot && output[write] != b'/' {
                    write -= 1;
                }
                output.truncate(write);
            } else if !rooted {
                if !output.is_empty() {
                    output.push(b'/');
                }
                output.extend_from_slice(b"..");
                dotdot = output.len();
            }
        } else {
            if (rooted && output.len() != 1) || (!rooted && !output.is_empty()) {
                output.push(b'/');
            }
            while read < path.len() && path[read] != b'/' {
                output.push(path[read]);
                read += 1;
            }
        }
    }

    if output.is_empty() {
        b".".to_vec()
    } else {
        output
    }
}

fn join_with_slash(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut joined = Vec::with_capacity(prefix.len() + 1 + suffix.len());
    joined.extend_from_slice(prefix);
    joined.push(b'/');
    joined.extend_from_slice(suffix);
    joined
}

/// Port of `sanitizedPathJoin` (`cgi.go:336-350`).
///
/// The untrusted request path is rooted and cleaned before it is joined to
/// the trusted root, so `..` segments cannot escape that root. Percent-encoded
/// separators are bytes, not path separators, and are deliberately untouched.
pub fn sanitized_path_join(root: &[u8], req_path: &[u8]) -> Vec<u8> {
    let root = if root.is_empty() {
        b".".as_slice()
    } else {
        root
    };
    let cleaned_request = clean_path(&join_with_slash(b"", req_path));
    let mut joined = clean_path(&join_with_slash(root, &cleaned_request));

    if req_path.len() > 1 && req_path.last() == Some(&b'/') {
        joined.push(b'/');
    }

    joined
}

/// The two schemes used for CGI's default `SERVER_PORT` derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// Byte-level port of Go's `net.SplitHostPort` success conditions.
fn split_host_port(host_port: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = host_port.iter().rposition(|&byte| byte == b':')?;
    let mut opening_bracket_offset = 0;
    let mut closing_bracket_offset = 0;

    let host = if host_port.first() == Some(&b'[') {
        let closing_bracket = host_port.iter().position(|&byte| byte == b']')?;
        match closing_bracket + 1 {
            end if end == host_port.len() => return None,
            end if end == colon => {}
            _ => return None,
        }
        opening_bracket_offset = 1;
        closing_bracket_offset = closing_bracket + 1;
        &host_port[1..closing_bracket]
    } else {
        let host = &host_port[..colon];
        if host.contains(&b':') {
            return None;
        }
        host
    };

    if host_port[opening_bracket_offset..].contains(&b'[')
        || host_port[closing_bracket_offset..].contains(&b']')
    {
        return None;
    }

    Some((host, &host_port[colon + 1..]))
}

/// Port of `splitRemoteAddr` (`cgi.go:357-374`).
///
/// It tries strict host/port parsing first, then falls back to the final
/// colon, and finally strips a matching bracket pair. Every access is guarded
/// so malformed network metadata cannot panic a future C callback.
pub fn split_remote_addr(remote_addr: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if let Some((host, port)) = split_host_port(remote_addr) {
        return (host.to_vec(), port.to_vec());
    }

    let (mut ip, port) = match remote_addr.iter().rposition(|&byte| byte == b':') {
        Some(colon) => (
            remote_addr[..colon].to_vec(),
            remote_addr[colon + 1..].to_vec(),
        ),
        None => (remote_addr.to_vec(), Vec::new()),
    };

    if ip.len() >= 2 && ip.first() == Some(&b'[') && ip.last() == Some(&b']') {
        ip = ip[1..ip.len() - 1].to_vec();
    }

    (ip, port)
}

/// Derives `SERVER_NAME` and `SERVER_PORT` from the request Host and scheme
/// (`cgi.go:71-92`).
///
/// A successful `SplitHostPort` strips IPv6 brackets. On any parse error, or
/// when the parsed host is empty, upstream falls back to Host verbatim. An
/// absent or empty port uses the scheme's CGI-mandated default.
pub fn derive_server_name_and_port(host: &[u8], scheme: Scheme) -> (Vec<u8>, Vec<u8>) {
    let parsed = split_host_port(host);
    let server_name = match parsed {
        Some((name, _)) if !name.is_empty() => name.to_vec(),
        _ => host.to_vec(),
    };
    let server_port = match parsed {
        Some((_, port)) if !port.is_empty() => port.to_vec(),
        _ => match scheme {
            Scheme::Http => b"80".to_vec(),
            Scheme::Https => b"443".to_vec(),
        },
    };

    (server_name, server_port)
}

/// Derives only `SERVER_NAME`; see [`derive_server_name_and_port`].
pub fn derive_server_name(host: &[u8]) -> Vec<u8> {
    match split_host_port(host) {
        Some((name, _)) if !name.is_empty() => name.to_vec(),
        _ => host.to_vec(),
    }
}

/// Derives only `SERVER_PORT`; see [`derive_server_name_and_port`].
pub fn derive_server_port(host: &[u8], scheme: Scheme) -> Vec<u8> {
    match split_host_port(host) {
        Some((_, port)) if !port.is_empty() => port.to_vec(),
        _ => match scheme {
            Scheme::Http => b"80".to_vec(),
            Scheme::Https => b"443".to_vec(),
        },
    }
}

/// The 101 common request headers from
/// `internal/phpheaders/phpheaders.go:15-118`.
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

fn valid_header_field_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
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

/// Byte-level `textproto.CanonicalMIMEHeaderKey`.
///
/// Invalid field-name bytes cause the input to be returned unchanged, just
/// as Go does. Valid names uppercase the first letter and letters following a
/// hyphen, and lowercase all other ASCII letters.
pub fn canonical_mime_header_name(name: &[u8]) -> Vec<u8> {
    if name.iter().any(|&byte| !valid_header_field_byte(byte)) {
        return name.to_vec();
    }

    let mut canonical = Vec::with_capacity(name.len());
    let mut uppercase = true;
    for &byte in name {
        let byte = if uppercase {
            byte.to_ascii_uppercase()
        } else {
            byte.to_ascii_lowercase()
        };
        canonical.push(byte);
        uppercase = byte == b'-';
    }
    canonical
}

/// Exact-match lookup for a canonical header name, matching Go's map access.
pub fn lookup_common_header(canonical_name: &[u8]) -> Option<&'static str> {
    COMMON_HEADERS
        .iter()
        .find_map(|&(name, variable)| (name.as_bytes() == canonical_name).then_some(variable))
}

/// Canonicalizes a header name before looking it up in [`COMMON_HEADERS`].
pub fn common_header_var(name: &[u8]) -> Option<&'static str> {
    lookup_common_header(&canonical_mime_header_name(name))
}

/// Uppercases an HTTP header name, replaces `-` with `_`, and prefixes it
/// with `HTTP_`, as `phpheaders.go` does for uncommon headers.
///
/// Underscores are deliberately preserved, so `Foo_Bar` and `Foo-Bar`
/// produce the same CGI variable name, matching upstream's documented
/// collision behavior.
pub fn mangle_header_name(name: &[u8]) -> Vec<u8> {
    let mut mangled = Vec::with_capacity(5 + name.len());
    mangled.extend_from_slice(b"HTTP_");
    mangled.extend(name.iter().map(|&byte| {
        if byte == b'-' {
            b'_'
        } else {
            byte.to_ascii_uppercase()
        }
    }));
    mangled
}

/// Returns the uncommon-header key expected by the bare C `char *key`
/// consumer. The result is always NUL-terminated.
pub fn uncommon_header_key(name: &[u8]) -> Vec<u8> {
    let mut key = mangle_header_name(name);
    key.push(0);
    key
}

/// Joins repeated header values exactly like Go's `strings.Join(values,
/// ", ")`, without requiring the values to be UTF-8.
pub fn join_header_values<V>(values: &[V]) -> Vec<u8>
where
    V: AsRef<[u8]>,
{
    let output_len = values
        .iter()
        .map(|value| value.as_ref().len())
        .sum::<usize>()
        + values.len().saturating_sub(1) * 2;
    let mut joined = Vec::with_capacity(output_len);
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            joined.extend_from_slice(b", ");
        }
        joined.extend_from_slice(value.as_ref());
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(ensure_leading_slash(input.as_bytes()), expected.as_bytes());
        }
    }

    #[test]
    fn split_remote_addr_matches_upstream_table() {
        let cases = [
            ("1.2.3.4:5", "1.2.3.4", "5"),
            ("[::1]:443", "::1", "443"),
            ("[fe80::1%eth0]:443", "fe80::1%eth0", "443"),
            ("192.168.0.1", "192.168.0.1", ""),
            ("", "", ""),
            (":", "", ""),
            ("[", "[", ""),
            ("[:9000", "[", "9000"),
            ("[]", "", ""),
            ("[:", "[", ""),
            ("[::1:80", "[::1", "80"),
        ];

        for (address, expected_ip, expected_port) in cases {
            let (ip, port) = split_remote_addr(address.as_bytes());
            assert_eq!(ip, expected_ip.as_bytes(), "IP for {address:?}");
            assert_eq!(port, expected_port.as_bytes(), "port for {address:?}");
        }
    }

    #[test]
    fn split_pos_matches_upstream_table() {
        let php = [b".php".as_slice()];
        let php_phtml = [b".php".as_slice(), b".phtml".as_slice()];
        let cases: &[(&str, &[&[u8]], isize)] = &[
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
            ("/script.p¡p", &php, -1),
            ("/shell﹒php", &php, -1),
            ("/shell．php", &php, -1),
            ("/shell.ｐhp", &php, -1),
            ("/shell.ⓟⓗⓟ", &php, -1),
            ("/shell.\u{1d5fd}\u{1d5f5}\u{1d5fd}", &php, -1),
            ("/shell.\u{1d4c5}\u{1d4bd}\u{1d4c5}", &php, -1),
            ("/shell.ⓟⓗⓟ.anything-after-payload.php", &php, 43),
        ];

        for (path, split_path, expected) in cases {
            assert_eq!(
                split_pos(path.as_bytes(), split_path),
                *expected,
                "path {path:?}"
            );
        }
    }

    #[test]
    fn split_pos_unicode_case_folding_length_expansion_regression() {
        let path = "/ȺȺȺȺshell.php.txt.php";
        assert_eq!(split_pos(path.as_bytes(), &[b".php".as_slice()]), 18);
    }

    #[test]
    fn split_pos_security_regression_unicode_bypass() {
        let payloads = [
            "/PoC-match-unset.¡.txt",
            "/shell﹒php",
            "/shell．php",
            "/shell.ｐhp",
            "/shell.pｈp",
            "/shell.phｐ",
            "/shell.\u{1d5c1}\u{1d5b5}\u{1d5c1}",
            "/shell.\u{1d5fd}\u{1d5f5}\u{1d5fd}",
            "/shell.\u{1d4c5}\u{1d4bd}\u{1d4c5}",
            "/shell.ⓟⓗⓟ",
        ];

        for payload in payloads {
            assert_eq!(
                split_pos(payload.as_bytes(), &[b".php".as_slice()]),
                -1,
                "payload {payload:?} must not match"
            );
        }
    }

    fn sanitized(root: &str, request_path: &str) -> Vec<u8> {
        sanitized_path_join(root.as_bytes(), request_path.as_bytes())
    }

    #[test]
    fn sanitized_path_join_rejects_traversal_and_absolute_request_paths() {
        let cases = [
            ("../../etc/passwd", "/var/www/etc/passwd"),
            ("/../../../etc/passwd", "/var/www/etc/passwd"),
            ("/a/../../b", "/var/www/b"),
            ("/index.php", "/var/www/index.php"),
            ("index.php", "/var/www/index.php"),
            ("/", "/var/www"),
        ];

        for (request_path, expected) in cases {
            let result = sanitized("/var/www", request_path);
            assert_eq!(result, expected.as_bytes());
            assert!(result == b"/var/www" || result.starts_with(b"/var/www/"));
        }
    }

    #[test]
    fn sanitized_path_join_preserves_encoded_separators_and_trailing_slashes() {
        let encoded = sanitized("/var/www", "/foo%2f..%2f..%2fetc%2fpasswd");
        assert_eq!(encoded, b"/var/www/foo%2f..%2f..%2fetc%2fpasswd");
        assert!(encoded.starts_with(b"/var/www/"));
        assert_eq!(sanitized("/var/www", "/dir/"), b"/var/www/dir/");
    }

    #[test]
    fn sanitized_path_join_matches_root_and_non_utf8_cases() {
        assert_eq!(sanitized("", "/etc/passwd"), b"etc/passwd");
        assert_eq!(sanitized("/var/www/..", "/index.php"), b"/var/index.php");
        assert_eq!(
            sanitized_path_join(b"/var/www", b"/\xff/../../etc/passwd"),
            b"/var/www/etc/passwd"
        );
        assert_eq!(
            sanitized_path_join(b"/var/www", b"/caf\xe9.php"),
            b"/var/www/caf\xe9.php"
        );
    }

    #[test]
    fn clean_path_matches_go_filepath_clean() {
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
            ("/var/www/..", "/var"),
            ("/a/b/..", "/a"),
            ("a/b/..", "a"),
            ("/a/b/../c", "/a/c"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                clean_path(input.as_bytes()),
                expected.as_bytes(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn split_path_validation_matches_upstream_and_enables_uppercase_config() {
        assert_eq!(
            normalize_split_path([".PhP", ".PHTML"]).unwrap(),
            [b".php".to_vec(), b".phtml".to_vec()]
        );
        assert_eq!(
            normalize_split_path([".php", ".Ⱥphp"]),
            Err(InvalidSplitPath)
        );

        let configured = SplitPath::new([".PHP"]).unwrap();
        assert_eq!(split_pos(b"/x.php", configured.as_slice()), 6);
    }

    #[test]
    fn split_cgi_path_distinguishes_absent_empty_and_configured_lists() {
        let absent = split_cgi_path(b"/var/www", None, b"/index.php/info");
        assert_eq!(absent.doc_uri, b"/index.php");
        assert_eq!(absent.path_info, b"/info");
        assert_eq!(absent.script_name, b"/index.php");
        assert_eq!(absent.script_filename, b"/var/www/index.php");

        let empty = SplitPath::new(Vec::<Vec<u8>>::new()).unwrap();
        let explicit_empty = split_cgi_path(b"/var/www", Some(&empty), b"/index.php");
        assert_eq!(explicit_empty.doc_uri, b"");
        assert_eq!(explicit_empty.path_info, b"/index.php");
        assert_eq!(explicit_empty.script_name, b"");
        assert_eq!(explicit_empty.script_filename, b"/var/www");

        let configured = SplitPath::new([".PHP"]).unwrap();
        let configured_paths = split_cgi_path(b"/srv", Some(&configured), b"/x.php/more");
        assert_eq!(configured_paths.doc_uri, b"/x.php");
        assert_eq!(configured_paths.path_info, b"/more");
        assert_eq!(configured_paths.script_name, b"/x.php");
        assert_eq!(configured_paths.script_filename, b"/srv/x.php");
    }

    #[test]
    fn server_name_and_port_match_upstream_host_parsing() {
        assert_eq!(derive_server_name(b"example.com:8080"), b"example.com");
        assert_eq!(derive_server_name(b"[::1]:80"), b"::1");
        assert_eq!(derive_server_name(b"[::1]"), b"[::1]");

        assert_eq!(derive_server_port(b"example.com", Scheme::Http), b"80");
        assert_eq!(derive_server_port(b"example.com", Scheme::Https), b"443");
        assert_eq!(
            derive_server_port(b"example.com:9000", Scheme::Https),
            b"9000"
        );
        assert_eq!(
            derive_server_name_and_port(b"[::1]:8443", Scheme::Https),
            (b"::1".to_vec(), b"8443".to_vec())
        );
    }

    #[test]
    fn header_canonicalization_and_common_lookup_match_go() {
        assert_eq!(
            canonical_mime_header_name(b"accept-encoding"),
            b"Accept-Encoding"
        );
        assert_eq!(
            common_header_var(b"accept-encoding"),
            Some("HTTP_ACCEPT_ENCODING")
        );
        assert_eq!(lookup_common_header(b"accept-encoding"), None);
        assert_eq!(
            canonical_mime_header_name(b"invalid header"),
            b"invalid header"
        );
    }

    #[test]
    fn header_mangler_agrees_with_all_101_common_entries() {
        assert_eq!(COMMON_HEADERS.len(), 101);
        assert_eq!(
            mangle_header_name(b"Accept-Encoding"),
            b"HTTP_ACCEPT_ENCODING"
        );
        assert_eq!(
            mangle_header_name(b"Foo_Bar"),
            mangle_header_name(b"Foo-Bar")
        );

        for &(name, variable) in COMMON_HEADERS {
            assert_eq!(
                mangle_header_name(name.as_bytes()),
                variable.as_bytes(),
                "{name}"
            );
            let key = uncommon_header_key(name.as_bytes());
            assert_eq!(key.last(), Some(&0), "{name} must be NUL-terminated");
            assert_eq!(&key[..key.len() - 1], variable.as_bytes(), "{name}");
            assert_eq!(lookup_common_header(name.as_bytes()), Some(variable));
        }
    }

    #[test]
    fn header_value_join_is_byte_oriented() {
        let values = [
            b"gzip".as_slice(),
            b"caf\xe9".as_slice(),
            b"\xff".as_slice(),
        ];
        assert_eq!(join_header_values(&values), b"gzip, caf\xe9, \xff");
        assert_eq!(join_header_values::<&[u8]>(&[]), b"");
    }
}
