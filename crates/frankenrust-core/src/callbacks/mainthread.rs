//! Main PHP pthread startup and shutdown callbacks.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::os::raw::{c_char, c_ulong};
use std::ptr;

use frankenrust_sys::{malloc, zend_array};

use crate::state::State;
use crate::thread::{main_thread, report_main_callback_return_for_test};

/// `frankenphp.c:1710`, inside `php_main()`. This callback deliberately owns
/// the main pthread until final shutdown or a coordinated reboot.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_main_thread_is_ready() {
    let Some(main) = main_thread() else {
        write_diagnostic(format_args!(
            "main-thread-ready callback has no installed registry"
        ));
        return;
    };

    // #103 replaces this named finalization seam with max_threads=auto
    // resolution. Metrics initialization waits for the Ready publication.
    main.finalize_max_threads();
    main.state_machine().set(State::Ready);
    main.state_machine()
        .wait_for(&[State::Done, State::Rebooting]);
    report_main_callback_return_for_test();
}

/// `frankenphp.c:1727`, after SAPI shutdown and `tsrm_shutdown()`.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_shutdown_main_thread() {
    let Some(main) = main_thread() else {
        write_diagnostic(format_args!(
            "main-thread-shutdown callback has no installed registry"
        ));
        return;
    };

    if !main
        .state_machine()
        .compare_and_swap(State::Rebooting, State::YieldingForReboot)
    {
        main.state_machine().set(State::Reserved);
    }
}

/// `frankenphp.c:1681`/`:1685`. C frees this allocation with libc `free()` at
/// `frankenphp.c:1723`.
#[unsafe(no_mangle)]
pub extern "C" fn go_get_custom_php_ini(disable_timeouts: bool) -> *mut c_char {
    let Some(main) = main_thread() else {
        write_diagnostic(format_args!(
            "custom-php.ini callback has no installed main thread"
        ));
        return ptr::null_mut();
    };

    let mut php_ini = main.php_ini();
    if disable_timeouts {
        // ZTS execution timers are broken on platforms which lack
        // ZEND_MAX_EXECUTION_TIMERS (`phpmainthread.go:294-300`).
        php_ini.insert("max_execution_time".to_string(), "0".to_string());
        php_ini.insert("max_input_time".to_string(), "-1".to_string());
    }
    malloc_ini_overrides(&php_ini)
}

fn malloc_ini_overrides(php_ini: &HashMap<String, String>) -> *mut c_char {
    let Some(capacity) = php_ini.iter().try_fold(1usize, |length, (key, value)| {
        length
            .checked_add(key.len())?
            .checked_add(value.len())?
            .checked_add(2)
    }) else {
        return ptr::null_mut();
    };

    let mut overrides = Vec::with_capacity(capacity);
    for (key, value) in php_ini {
        overrides.extend_from_slice(key.as_bytes());
        overrides.push(b'=');
        overrides.extend_from_slice(value.as_bytes());
        overrides.push(b'\n');
    }
    overrides.push(0);

    let Ok(allocation_size) = c_ulong::try_from(overrides.len()) else {
        return ptr::null_mut();
    };

    // SAFETY: the bindgen declaration is libc malloc from stdlib.h. The size
    // is the nonzero Vec length (it includes a trailing NUL), and ownership is
    // transferred to C, which calls the matching free at frankenphp.c:1723.
    let allocation = unsafe { malloc(allocation_size) }.cast::<c_char>();
    if allocation.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: `allocation` names `overrides.len()` writable malloc bytes; the
    // Vec source is valid for the same length, and the independent allocations
    // cannot overlap.
    unsafe {
        ptr::copy_nonoverlapping(
            overrides.as_ptr().cast::<c_char>(),
            allocation,
            overrides.len(),
        );
    }
    allocation
}

/// `frankenphp.c:1698`, once while C initializes its persistent environment
/// table.
///
/// This intentionally leaves the environment snapshot thin. Reproducing
/// Go's raw `os.Environ` duplicate/no-`=` semantics is #98; `vars_os` cannot do
/// so faithfully. The persistent-string binding is exposed for that follow-up.
#[unsafe(no_mangle)]
pub extern "C" fn go_init_os_env(_main_thread_env: *mut zend_array) {}

fn write_diagnostic(arguments: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "frankenrust: {arguments}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{install_test_main, remove_test_main, TEST_REGISTRY};
    use frankenrust_sys::free;
    use std::ffi::CStr;

    #[test]
    fn custom_php_ini_uses_libc_memory_and_honors_disable_timeouts() {
        let _serial = TEST_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let main = install_test_main(HashMap::from([
            ("alpha".to_string(), "one".to_string()),
            ("beta".to_string(), "two".to_string()),
        ]));

        let rendered = go_get_custom_php_ini(false);
        assert!(!rendered.is_null());
        // SAFETY: the callback returned a NUL-terminated malloc allocation. It
        // remains live until the matching free below.
        let text = unsafe { CStr::from_ptr(rendered) }.to_string_lossy();
        let mut lines: Vec<_> = text.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["alpha=one", "beta=two"]);
        // SAFETY: this exact pointer came from bindgen's libc malloc in
        // `go_get_custom_php_ini` and ownership was transferred to the caller.
        unsafe { free(rendered.cast::<std::ffi::c_void>()) };

        let with_timeouts_disabled = go_get_custom_php_ini(true);
        assert!(!with_timeouts_disabled.is_null());
        // SAFETY: same allocation contract as `rendered` above.
        let text = unsafe { CStr::from_ptr(with_timeouts_disabled) }.to_string_lossy();
        let lines: Vec<_> = text.lines().collect();
        assert!(lines.contains(&"max_execution_time=0"));
        assert!(lines.contains(&"max_input_time=-1"));
        // SAFETY: this pointer came from the same libc-malloc callback path.
        unsafe { free(with_timeouts_disabled.cast::<std::ffi::c_void>()) };

        remove_test_main(&main);
    }
}
