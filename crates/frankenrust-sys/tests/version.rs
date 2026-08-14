//! Proves the whole chain actually works: `frankenphp.c` and `types.c`
//! compiled, linked against libphp, and the resulting code runs; and that
//! all 26 `go_*` symbols it calls back into resolved to a real definition
//! -- from `frankenrust-core`'s abort-stubs for 25 of them, and from
//! `frankenrust-sys/shim.c` for `go_register_server_variables` (issue #11:
//! it is the one callback whose C-ABI entry point had to move out of Rust) --
//! rather than staying undefined. See issue #7's Acceptance section.
//!
//! `frankenrust-core` is a dev-dependency (see frankenrust-sys/Cargo.toml)
//! purely so this binary links: `frankenphp.c`'s compiled object
//! unconditionally references all 26 symbols, and frankenrust-core is the
//! only crate that defines them.
//!
//! Two things have to stay reachable for the linker's default
//! `--gc-sections` not to prune them before the nm check below ever sees
//! them: (1) frankenphp.c's own call sites for the go_* symbols, and (2)
//! frankenrust-core's rlib being part of this link at all. Calling
//! `frankenphp_get_version()` alone (the acceptance test's other half)
//! reaches neither -- it is a leaf that calls nothing else -- so the two
//! `#[used] static`s below take the address of real entry points instead
//! (never calling them, which would trip the abort-stubs):
//! `frankenphp_new_main_thread`/`frankenphp_new_php_thread` are what a real
//! caller would use to spawn PHP's main/worker threads, and their internal
//! call graphs (`php_main()`/`php_thread()`, `frankenphp.c:1471-1730`) between
//! them reach all 26 go_* call sites. `#[used]` (not a plain unused `const`,
//! which the compiler is free to discard entirely if never read) is what
//! guarantees the reference survives both Rust's own dead-code elimination
//! and the linker's section-level one.
use std::collections::HashSet;
use std::os::raw::c_int;
use std::process::Command;

#[used]
static KEEP_FRANKENPHP_NEW_MAIN_THREAD: unsafe extern "C" fn(c_int) -> c_int =
    frankenrust_sys::frankenphp_new_main_thread;
#[used]
static KEEP_FRANKENPHP_NEW_PHP_THREAD: unsafe extern "C" fn(usize) -> bool =
    frankenrust_sys::frankenphp_new_php_thread;

/// Referencing one symbol from frankenrust_core is enough for Cargo to pass
/// its compiled rlib to the linker for this binary at all; from there, the
/// linker's mark-sweep over every provided archive resolves the go_*
/// symbols the two statics above kept alive on the frankenphp.c side,
/// demand-driven by ordinary C-ABI static-library linking (nothing
/// Rust-specific). Takes the function's address only -- never calls it, so
/// this never triggers the abort-stub.
#[used]
static TOUCH_FRANKENRUST_CORE: extern "C" fn() =
    frankenrust_core::callbacks::mainthread::go_frankenphp_main_thread_is_ready;

const GO_SYMBOLS: [&str; 26] = [
    // callbacks/thread.rs
    "go_frankenphp_store_force_kill_slot",
    "go_frankenphp_before_script_execution",
    "go_frankenphp_after_script_execution",
    "go_frankenphp_clear_force_kill_slot",
    "go_frankenphp_on_thread_shutdown",
    // callbacks/output.rs
    "go_ub_write",
    "go_write_headers",
    "go_sapi_flush",
    // callbacks/input.rs
    "go_read_post",
    "go_read_cookies",
    "go_apache_request_headers",
    // callbacks/servervars.rs
    "go_register_server_variables",
    "go_update_request_info",
    // callbacks/mainthread.rs
    "go_frankenphp_main_thread_is_ready",
    "go_frankenphp_shutdown_main_thread",
    "go_get_custom_php_ini",
    "go_init_os_env",
    // callbacks/worker.rs
    "go_frankenphp_worker_handle_request_start",
    "go_frankenphp_finish_worker_request",
    "go_frankenphp_finish_php_request",
    // callbacks/log.rs
    "go_log",
    "go_log_attrs",
    // callbacks/misc.rs
    "go_is_context_done",
    "go_putenv",
    "go_schedule_opcache_reset",
    "go_mercure_publish",
];

#[test]
fn frankenphp_get_version_reports_a_real_php() {
    // SAFETY: frankenphp_get_version() (frankenphp.c:83-88) takes no
    // arguments and returns a plain-old-data struct built entirely from
    // compile-time PHP_VERSION* constants -- no PHP/Zend/TSRM state needs
    // to be initialised first, and it is safe to call from any thread.
    let version = unsafe { frankenrust_sys::frankenphp_get_version() };

    assert!(
        version.major_version >= 1,
        "major_version = {}",
        version.major_version
    );
    assert_ne!(version.version_id, 0, "version_id should not be zero");
}

#[test]
fn all_26_go_symbols_are_defined_in_the_test_binary() {
    let exe =
        std::env::current_exe().expect("current_exe() should resolve for a `cargo test` binary");

    // --defined-only: a symbol that failed to resolve wouldn't appear here at
    // all (and, for that matter, this binary would not have linked in the
    // first place -- this is a belt-and-suspenders check, not the only line
    // of defence). GNU nm, per docker/frankenrust-dev.Dockerfile (Linux only).
    let output = Command::new("nm")
        .arg("--defined-only")
        .arg(&exe)
        .output()
        .expect("failed to run `nm` (see docker/frankenrust-dev.Dockerfile -- GNU binutils)");
    assert!(
        output.status.success(),
        "nm {} failed: {}",
        exe.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let defined: HashSet<&str> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .collect();

    for symbol in GO_SYMBOLS {
        assert!(
            defined.contains(symbol),
            "{symbol} is not a defined symbol in {} -- it should be defined by \
             frankenrust-core/src/callbacks/*.rs, except go_register_server_variables, \
             which is defined by frankenrust-sys/shim.c (issue #11)",
            exe.display()
        );
    }
}
