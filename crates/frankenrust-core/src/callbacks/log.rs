//! The logging facade -- issue #106's own design decision, not a detail of
//! either callback below (see the issue body's "The logging facade"
//! section: `frankenrust-core` had no logging of any kind before this) --
//! plus `go_log`, the SAPI error-log hook
//! (`vendor/frankenphp/frankenphp.c:1385-1388`, wired up as
//! `sapi_module_struct.log_message`; ported from `frankenphp.go:741-767`).
//!
//! `go_log_attrs` -- the structured `frankenphp_log()` userland function
//! (`frankenphp.c:998-1004`, `:1586`) -- stays exactly the abort-stub issue
//! #7 left it: widening `frankenrust-sys`'s FFI surface to walk a
//! `zend_array` of arbitrary PHP values is out of this issue's scope (the
//! resolver split it out as issue #109), which builds its structured attrs
//! on top of the facade below.
//!
//! # The facade
//!
//! [`Level`] is a signed offset on `log/slog`'s numeric scale, not a
//! four-variant enum, so it can represent whatever raw level #109's
//! `go_log_attrs` passes through -- see [`Level`]'s doc comment. [`enabled`]
//! is the cheap, formatting-free check every log site should run before
//! doing any work; [`log`] runs it automatically before invoking either of
//! its closures, so a disabled level costs one integer comparison and
//! nothing else. [`log_once`] adds a `std::sync::Once` gate on top, for the
//! unsupported-feature sentinels in `misc.rs`.
//!
//! There is no per-request logger here, and this issue deliberately does not
//! add one: this crate's `RequestContext` (`context.rs`) has no logger
//! field, so every call site in this crate logs through the same global sink
//! regardless of whether a request is installed for the calling thread.
//! That is a narrower, and strictly stronger, version of upstream's
//! `getLogger` degradation (`frankenphp.go:722-739`, which falls back to a
//! global logger only once thread/handler/context/per-request-logger are
//! each individually checked and found missing): here there is simply
//! nothing to fall back *from*, so "no current request installed" and
//! "mid-request" are the same code path for every callback in this module.
//!
//! Records are written to stderr in production. Tests capture them instead
//! -- see [`capture`] -- so assertions can pin exact levels, message bytes
//! and attributes without scraping process output.

use std::ffi::CStr;
use std::io::Write as _;
use std::os::raw::{c_char, c_int};
use std::sync::Once;

use frankenrust_sys::{zend_long, zend_string, zval};

use super::abort_stub;

/// A log level on `log/slog`'s numeric scale: `Level(0)` is `slog.LevelInfo`,
/// and the four named levels below match `slog`'s own named constants
/// exactly (`slog.LevelDebug = -4`, `slog.LevelInfo = 0`, `slog.LevelWarn =
/// 4`, `slog.LevelError = 8`).
///
/// Kept as a signed offset rather than a four-variant enum because
/// `go_log_attrs` (issue #109, still an abort-stub in this file) receives
/// PHP's raw `zend_long` level unmodified (`frankenphp.go:769-791`:
/// `level := slog.Level(cLevel)`) and passes it straight to
/// `logger.Enabled`/`LogAttrs` -- `slog`'s own `Level.String()` renders an
/// in-between value like `2` as `"INFO+2"` rather than rounding to the
/// nearest named level, and `Enabled` compares numerically against a
/// handler's configured minimum. This type has to carry that same numeric
/// axis to be able to represent whatever #109 hands it -- [`Level::from_raw`]
/// is the conversion #109 will use, and it preserves the raw value exactly
/// rather than clamping it to one of the four constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(i64);

impl Level {
    pub const DEBUG: Level = Level(-4);
    pub const INFO: Level = Level(0);
    pub const WARN: Level = Level(4);
    pub const ERROR: Level = Level(8);

    /// A raw slog-scale level -- e.g. the numeric value #109's
    /// `go_log_attrs` will read out of a PHP `zend_long`. Intermediate and
    /// out-of-range values are legal input on `slog`'s own scale and are
    /// preserved exactly, not clamped or rounded to the nearest named level.
    pub const fn from_raw(raw: i64) -> Level {
        Level(raw)
    }

    /// The inverse of [`Level::from_raw`], for a caller that needs the
    /// numeric value back (e.g. to format it the way `slog.Level.String()`
    /// would for an in-between level).
    pub const fn as_raw(self) -> i64 {
        self.0
    }
}

/// One structured key/value pair on a [`Record`] -- this facade's analogue
/// of a single `slog.Attr`. String-valued only: the one attribute this issue
/// produces (`go_log`'s `syslog_level`) is a string, and widening this to
/// arbitrary `slog.Value`-like data is #109's problem to solve when it walks
/// a PHP `zend_array` of arbitrary values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub key: &'static str,
    pub value: String,
}

/// One emitted log record. `message` is `Vec<u8>`, not `String`: PHP strings
/// are arbitrary bytes (`docs/PORTING-NOTES.md`'s construct-mapping table),
/// and `go_log`'s message in particular is a raw `*mut c_char` from PHP with
/// no encoding guarantee at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub level: Level,
    pub message: Vec<u8>,
    pub attrs: Vec<Attr>,
}

/// The minimum level this facade emits. There is no configuration plumbing
/// in this crate yet -- no CLI flags, no Caddyfile, no per-request logger,
/// see this module's doc comment -- so this is a fixed default matching
/// `slog`'s own default when a program builds a `Logger` with no explicit
/// `HandlerOptions.Level`: `slog.LevelInfo`. Making this runtime-configurable
/// is later work, for whichever issue wires up real configuration.
const MIN_LEVEL: Level = Level::INFO;

/// Cheap, and deliberately does nothing else: this is the check upstream's
/// own call sites run *before* building a message (`frankenphp.go:466-471`,
/// `:591-597`, `:648-655`, all guards in front of a `getLogger` call).
/// [`log`] already calls this before invoking either of its closures, so a
/// call site that only ever goes through [`log`] never needs to call this
/// itself -- it is exposed separately because a facade whose only entry
/// point formats eagerly is the wrong shape (see this module's doc
/// comment), and a future call site may need to skip more than message
/// formatting ahead of the same check (`go_log_attrs`, #109, additionally
/// has to walk a PHP `zend_array` to build its attrs).
pub fn enabled(level: Level) -> bool {
    level >= MIN_LEVEL
}

/// Emits a record at `level`, formatting nothing and building no attrs if
/// `level` is not [`enabled`]. `message` and `attrs` are closures precisely
/// so that a disabled level costs one integer comparison and nothing else.
pub fn log(level: Level, message: impl FnOnce() -> Vec<u8>, attrs: impl FnOnce() -> Vec<Attr>) {
    if !enabled(level) {
        return;
    }
    emit(Record {
        level,
        message: message(),
        attrs: attrs(),
    });
}

/// [`log`], but only the first call through a given `once` ever reaches it --
/// for the unsupported-feature sentinels in `misc.rs`: tell the operator
/// once per process that a feature is unavailable, not once per request. One
/// `std::sync::Once`, declared as its own `static` alongside the call site
/// it guards, is the whole primitive; this function only saves each call
/// site from re-deriving the `call_once` wrapper around [`log`].
///
/// The [`enabled`] check runs *before* `once.call_once`, not inside it: if it
/// ran inside, a disabled level would still consume the `Once` (`call_once`
/// counts as "run" the moment its closure returns, whether or not [`log`]
/// decided to emit anything), so the very first call at a disabled level
/// would silently and permanently disarm this site -- every level is enabled
/// today (`MIN_LEVEL` is fixed and every call site here logs at
/// [`Level::WARN`]), so this cannot yet happen, but the day `MIN_LEVEL`
/// becomes configurable it would turn "log once" into "never log" with no
/// visible error.
pub fn log_once(
    once: &Once,
    level: Level,
    message: impl FnOnce() -> Vec<u8>,
    attrs: impl FnOnce() -> Vec<Attr>,
) {
    if !enabled(level) {
        return;
    }
    once.call_once(|| log(level, message, attrs));
}

/// The only place a [`Record`] is actually written or captured -- everything
/// above this line decides *whether* to call it; nothing below it decides
/// that again.
fn emit(record: Record) {
    if try_capture(&record) {
        return;
    }
    write_stderr(&record);
}

fn write_stderr(record: &Record) {
    let mut line = format!(
        "frankenrust[{:?}]: {}",
        record.level,
        escape_for_log_line(&record.message)
    );
    for attr in &record.attrs {
        line.push_str(&format!(
            " {}={}",
            attr.key,
            escape_for_log_line(attr.value.as_bytes())
        ));
    }
    // A write failure here (closed pipe, ENOSPC, a full stderr buffer on a
    // blocking fd) must not propagate as a panic: this runs synchronously
    // from every `extern "C"` callback in this crate, none of which catches
    // unwinds, so a panic here would abort the whole server -- including
    // every in-flight request on every other thread -- over a lost log line.
    // `eprintln!` panics on write failure; `writeln!` does not, and the
    // `Result` is discarded deliberately, not out of laziness: there is
    // nothing more useful to do with a broken diagnostic sink than drop the
    // line and keep serving. Upstream's own `slog` handlers do the same --
    // `log/slog.Logger.log` discards its handler's write error
    // (`$GOROOT/src/log/slog/logger.go:256,276`, both `_ = l.Handler().Handle(...)`).
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// Renders `bytes` as an unambiguous, single-line-safe fragment of a stderr
/// log line. PHP strings are arbitrary bytes with no encoding guarantee (see
/// this module's doc comment), and a naive `String::from_utf8_lossy` plus
/// direct interpolation has two failure modes this function exists to avoid:
///
/// - **Log-line forging.** A message containing its own `\n` would start a
///   second, attacker-controlled line in the log output -- e.g. a PHP value
///   containing `"ok\nfrankenrust[Level(8)]: forged"` would otherwise render
///   as two lines, the second indistinguishable from a real record.
/// - **Silent data loss.** `from_utf8_lossy` maps every invalid byte to the
///   same replacement character (U+FFFD), so two messages that differ only
///   in which invalid byte they contain (say `0x80` vs `0x81`) become
///   identical in the log -- exactly the diagnostic data a log line exists
///   to preserve.
///
/// Every byte outside printable, non-backslash ASCII is therefore rendered
/// as an explicit, reversible escape (`\n`, `\t`, `\xHH`, ...) rather than
/// written raw or replaced. This also hex-escapes valid multi-byte UTF-8
/// (e.g. non-ASCII text renders as a run of `\xHH`s instead of the original
/// characters); that is a deliberate trade of readability for the guarantee
/// that no two distinct byte strings ever render identically and no message
/// can inject a line break.
fn escape_for_log_line(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

#[cfg(test)]
fn try_capture(record: &Record) -> bool {
    capture::push(record.clone())
}

#[cfg(not(test))]
fn try_capture(_record: &Record) -> bool {
    false
}

/// Test-only capture of emitted records, thread-local so `cargo test`'s
/// concurrent test execution cannot make two tests' assertions race over one
/// shared buffer -- a single global `Vec` would make these assertions flaky
/// rather than failing honestly. `pub(crate)` (not private) so sibling
/// callback modules' own test code (`misc.rs`) can capture through the same
/// facade their production code logs through.
#[cfg(test)]
pub(crate) mod capture {
    use std::cell::RefCell;

    use super::Record;

    std::thread_local! {
        static RECORDS: RefCell<Option<Vec<Record>>> = const { RefCell::new(None) };
    }

    /// Runs `f` with this thread's capture buffer open, and returns `f`'s
    /// result together with every record [`super::log`]/[`super::log_once`]
    /// emitted on this thread during the call.
    pub(crate) fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<Record>) {
        RECORDS.with(|records| *records.borrow_mut() = Some(Vec::new()));
        let result = f();
        let captured = RECORDS
            .with(|records| records.borrow_mut().take())
            .unwrap_or_default();
        (result, captured)
    }

    /// Appends `record` to the active capture buffer, if this thread has one
    /// open, and reports whether it did -- so [`super::emit`] knows whether
    /// it still needs to reach the real sink.
    pub(crate) fn push(record: Record) -> bool {
        RECORDS.with(|records| {
            let mut records = records.borrow_mut();
            match records.as_mut() {
                Some(records) => {
                    records.push(record);
                    true
                }
                None => false,
            }
        })
    }
}

/// The syslog priority levels PHP's SAPI `log_message()` hook passes as a
/// raw `int` (`frankenphp.c:1386`, `frankenphp_log_message()`), RFC 5424
/// §6.2.1 numbering, ported from `frankenphp.go:84-95`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogLevel {
    Emerg,
    Alert,
    Crit,
    Err,
    Warn,
    Notice,
    Info,
    Debug,
}

impl SyslogLevel {
    /// `frankenphp.go:745-748`: an out-of-range raw value falls back to
    /// `syslogLevelInfo` *before* the level-name mapping in
    /// [`SyslogLevel::level`] runs -- so e.g. `42` lands on [`Level::INFO`],
    /// not [`Level::ERROR`], even though `Err`'s syslog value (3) is
    /// numerically closer to it than `Info`'s (6) is.
    pub fn from_raw(raw: c_int) -> SyslogLevel {
        match raw {
            0 => SyslogLevel::Emerg,
            1 => SyslogLevel::Alert,
            2 => SyslogLevel::Crit,
            3 => SyslogLevel::Err,
            4 => SyslogLevel::Warn,
            5 => SyslogLevel::Notice,
            6 => SyslogLevel::Info,
            7 => SyslogLevel::Debug,
            _ => SyslogLevel::Info,
        }
    }

    /// `frankenphp.go:750-759`.
    pub fn level(self) -> Level {
        match self {
            SyslogLevel::Emerg | SyslogLevel::Alert | SyslogLevel::Crit | SyslogLevel::Err => {
                Level::ERROR
            }
            SyslogLevel::Warn => Level::WARN,
            SyslogLevel::Notice | SyslogLevel::Info => Level::INFO,
            SyslogLevel::Debug => Level::DEBUG,
        }
    }

    /// `le.String()` (`frankenphp.go:97-113`) -- the syslog level name
    /// attached as a structured attribute at the real call site
    /// (`frankenphp.go:766`: `slog.String("syslog_level", le.String())`).
    /// This is how [`go_log`] preserves the original syslog level once it
    /// has been folded down to one of our four [`Level`]s.
    pub fn name(self) -> &'static str {
        match self {
            SyslogLevel::Emerg => "emerg",
            SyslogLevel::Alert => "alert",
            SyslogLevel::Crit => "crit",
            SyslogLevel::Err => "err",
            SyslogLevel::Warn => "warning",
            SyslogLevel::Notice => "notice",
            SyslogLevel::Info => "info",
            SyslogLevel::Debug => "debug",
        }
    }
}

/// `frankenphp.c:1386`, inside `frankenphp_log_message()`
/// (`sapi_module_struct.log_message`). Ported from `frankenphp.go:741-767`.
///
/// Deliberately stricter than upstream in one respect, and identical in
/// effect for a different reason. Upstream reaches this through `getLogger`
/// (`frankenphp.go:722-739`), which is fully defensive about a missing
/// thread/handler/per-request-logger and falls back to a *global* logger and
/// context the moment any one of those is absent. This crate's
/// `RequestContext` (`context.rs`) carries no per-request logger field at
/// all -- see this module's doc comment -- so there is no per-request state
/// for `go_log` to read in the first place: it always logs through this
/// module's single global sink, whether or not a request is installed for
/// `thread_index`. "No current request installed" and "mid-request" are
/// therefore the same code path here, which is a stronger property than
/// upstream's degrade-to-global fallback, not a weaker one.
///
/// # Safety
///
/// `message` may be NULL, which is read as an empty message rather than
/// dereferenced -- see the NULL check in the body. If it is non-NULL it must
/// point to a NUL-terminated byte buffer, valid for reads and not mutated or
/// freed for the duration of this call. The sole caller,
/// `frankenphp_log_message` (`frankenphp.c:1385-1388`, wired up as
/// `sapi_module_struct.log_message`), always supplies exactly that: PHP's own
/// `const char *message` for the duration of one `log_message()` invocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_log(_thread_index: usize, message: *mut c_char, level: c_int) {
    let syslog_level = SyslogLevel::from_raw(level);

    log(
        syslog_level.level(),
        || {
            // Upstream reads this with `C.GoString(message)`
            // (`frankenphp.go:766`), and cgo's `GoString` yields `""` for a
            // nil pointer -- so upstream is nil-safe here by construction and
            // we have to check explicitly to match it. No caller in
            // `frankenphp.c` passes NULL today (`frankenphp_log_message` is
            // only installed as `sapi_module_struct.log_message`, and every
            // path into it in PHP's `php_log_err_with_severity` passes an
            // `spprintf`-built buffer), but this is the SAPI error-log hook:
            // it fires during `MINIT`, `opcache.preload` and
            // `php_module_shutdown()`, and losing the oracle's nil-tolerance
            // there would turn a would-be empty log line into a segfault.
            if message.is_null() {
                return Vec::new();
            }
            // SAFETY: `message` is non-NULL by the check above. This closure
            // only runs from inside `log`, which is called synchronously and
            // returns before `go_log` does, so `message` is still within the
            // validity window this function's own `# Safety` section requires
            // of its caller. We copy the bytes into an owned `Vec<u8>` here
            // and never retain the pointer itself. PHP strings are arbitrary
            // bytes, so this reads `.to_bytes()`, never `.to_str().unwrap()`.
            unsafe { CStr::from_ptr(message) }.to_bytes().to_vec()
        },
        || {
            vec![Attr {
                key: "syslog_level",
                value: syslog_level.name().to_string(),
            }]
        },
    );
}

/// `frankenphp.c:998`, inside `PHP_FUNCTION(frankenphp_log)`, and `:1586`,
/// on `php_thread()`'s unhealthy-thread (`zend_catch`) path.
///
/// Left exactly as issue #7's abort-stub -- see this module's doc comment
/// for why, and issue #109 for the real implementation.
#[unsafe(no_mangle)]
pub extern "C" fn go_log_attrs(
    _thread_index: usize,
    _message: *mut zend_string,
    _c_level: zend_long,
    _c_attrs: *mut zval,
) -> *mut c_char {
    abort_stub("go_log_attrs")
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::ptr;

    use super::*;

    #[test]
    fn syslog_level_mapping_matches_upstreams_table() {
        let cases = [
            (0, Level::ERROR), // emerg
            (3, Level::ERROR), // err
            (4, Level::WARN),  // warn
            (5, Level::INFO),  // notice
            (7, Level::DEBUG), // debug
        ];
        for (raw, want) in cases {
            assert_eq!(
                SyslogLevel::from_raw(raw).level(),
                want,
                "syslog level {raw} should map to {want:?}"
            );
        }
    }

    #[test]
    fn out_of_range_syslog_level_lands_on_info_not_error() {
        for raw in [-1, 8, 42, i32::MAX] {
            assert_eq!(
                SyslogLevel::from_raw(raw).level(),
                Level::INFO,
                "out-of-range syslog level {raw} must fall back to Info -- \
                 frankenphp.go:745-748 applies the syslogLevelInfo fallback \
                 before the level-name mapping runs, not after"
            );
        }
    }

    #[test]
    fn go_log_captures_level_and_arbitrary_bytes() {
        // 0xC3 with no valid UTF-8 continuation byte after it: not valid
        // UTF-8, and must survive into the captured record unchanged anyway.
        let raw_message: &[u8] = b"caf\xC3 broken utf8";
        let c_message = CString::new(raw_message).expect("no interior NUL");

        // SAFETY: `c_message` is a valid, NUL-terminated, live `CString` for
        // the whole call -- exactly what `go_log`'s `# Safety` section
        // requires of `message`.
        let (_, records) = capture::capture(|| unsafe {
            go_log(0, c_message.as_ptr().cast_mut(), 3 /* err */);
        });

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].level, Level::ERROR);
        assert_eq!(records[0].message, raw_message);
    }

    /// The gap two reviewers caught and zero tests did: `go_log` used to hand
    /// `message` straight to `CStr::from_ptr`, which is undefined behaviour on
    /// NULL, where upstream's `C.GoString(message)` (`frankenphp.go:766`)
    /// yields `""` for a nil pointer and is therefore nil-safe by
    /// construction. No caller in `frankenphp.c` passes NULL today, so no
    /// conformance run can reach this -- a direct call is the only thing that
    /// can, which is precisely why it needs a unit test rather than being left
    /// to the contract in the `# Safety` section.
    #[test]
    fn go_log_tolerates_a_null_message() {
        // SAFETY: `go_log`'s `# Safety` section admits NULL explicitly, and
        // reads it as an empty message; that contract is what is under test.
        let (_, records) = capture::capture(|| unsafe {
            go_log(0, ptr::null_mut(), 3 /* err */);
        });

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].level, Level::ERROR);
        assert!(
            records[0].message.is_empty(),
            "a NULL message must log as empty, the way C.GoString(nil) does, \
             not be dereferenced; got {:?}",
            records[0].message
        );
        // The rest of the record still has to be well-formed -- a NULL
        // message must not cost the syslog_level attribute too.
        assert_eq!(records[0].attrs.len(), 1);
        assert_eq!(records[0].attrs[0].value, "err");
    }

    #[test]
    fn go_log_attaches_the_syslog_level_name() {
        let c_message = CString::new("degraded").expect("no interior NUL");

        // SAFETY: see the identical call above.
        let (_, records) = capture::capture(|| unsafe {
            go_log(0, c_message.as_ptr().cast_mut(), 4 /* warn */);
        });

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].attrs.len(), 1);
        assert_eq!(records[0].attrs[0].key, "syslog_level");
        assert_eq!(records[0].attrs[0].value, "warning");
    }

    #[test]
    fn disabled_level_does_not_format_the_message_or_build_attrs() {
        let (_, records) = capture::capture(|| {
            log(
                Level::DEBUG,
                || panic!("message must not be formatted for a disabled level"),
                || panic!("attrs must not be built for a disabled level"),
            );
        });

        assert!(records.is_empty());
    }

    /// The bug two reviewers caught: `log_once` used to call `once.call_once`
    /// unconditionally, so a disabled level still consumed the `Once` -- the
    /// very first call at a disabled level would permanently disarm the
    /// site, and every later call, even at an enabled level, would then
    /// silently log nothing. Every level is enabled today (`MIN_LEVEL` is
    /// fixed), so this could not yet manifest through any real call site;
    /// this test drives `log_once` directly with `Level::DEBUG`, which is
    /// below `MIN_LEVEL`, to prove the `Once` survives a disabled call.
    #[test]
    fn log_once_does_not_consume_the_once_on_a_disabled_level() {
        let once = Once::new();

        let (_, disabled_records) = capture::capture(|| {
            log_once(
                &once,
                Level::DEBUG,
                || panic!("message must not be formatted for a disabled level"),
                || panic!("attrs must not be built for a disabled level"),
            );
        });
        assert!(disabled_records.is_empty());

        let (_, enabled_records) = capture::capture(|| {
            log_once(&once, Level::WARN, || b"now enabled".to_vec(), Vec::new);
        });
        assert_eq!(
            enabled_records.len(),
            1,
            "a disabled call must not have consumed the Once -- the next \
             enabled call through the same Once must still log"
        );
    }

    #[test]
    fn escape_for_log_line_escapes_newlines_so_a_message_cannot_forge_a_second_line() {
        let escaped = escape_for_log_line(b"ok\nfrankenrust[Level(8)]: forged, not a real record");

        assert!(
            !escaped.contains('\n'),
            "an escaped line must contain no raw newline; got {escaped:?}"
        );
        assert_eq!(
            escaped,
            "ok\\nfrankenrust[Level(8)]: forged, not a real record"
        );
    }

    #[test]
    fn escape_for_log_line_disambiguates_distinct_invalid_utf8_bytes() {
        // 0x80 and 0x81 are both lone UTF-8 continuation bytes -- invalid on
        // their own, and both collapse to the same U+FFFD under
        // `String::from_utf8_lossy`. They must not collapse here.
        let a = escape_for_log_line(&[0x80]);
        let b = escape_for_log_line(&[0x81]);

        assert_ne!(a, b, "distinct invalid bytes must render distinctly");
        assert_eq!(a, "\\x80");
        assert_eq!(b, "\\x81");
    }

    #[test]
    fn escape_for_log_line_passes_printable_ascii_through_unchanged() {
        assert_eq!(
            escape_for_log_line(b"caf\xC3 broken utf8"),
            "caf\\xc3 broken utf8"
        );
        assert_eq!(escape_for_log_line(b"plain text 123"), "plain text 123");
    }
}
