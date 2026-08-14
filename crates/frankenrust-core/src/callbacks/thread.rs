//! Lifecycle callbacks invoked by `php_thread()` on its owning pthread.

use std::fmt;
use std::io::{self, Write};
use std::os::raw::{c_char, c_int};
use std::ptr;

use frankenrust_sys::{force_kill_slot, frankenphp_release_thread_for_kill};

use crate::thread::thread_by_index;

/// Stores the force-kill slot captured on this PHP pthread immediately after
/// `ts_resource(0)`.
///
/// # Safety
///
/// `slot` must be the value produced by
/// `frankenphp_register_thread_for_kill` on the PHP pthread which owns
/// `thread_index`, and this callback must run before that pthread calls
/// `ts_free_thread()`. Its raw pointers are retained and later written through
/// by `frankenphp_force_kill_thread`.
///
/// ```compile_fail
/// use frankenrust_core::callbacks::thread::go_frankenphp_store_force_kill_slot;
/// use frankenrust_sys::force_kill_slot;
///
/// let forged: force_kill_slot = unsafe { std::mem::zeroed() };
/// go_frankenphp_store_force_kill_slot(0, forged);
/// ```
#[unsafe(no_mangle)]
pub unsafe extern "C" fn go_frankenphp_store_force_kill_slot(
    thread_index: usize,
    slot: force_kill_slot,
) {
    let Some(thread) = thread_by_index(thread_index) else {
        write_diagnostic(format_args!(
            "force-kill slot supplied for unknown thread {thread_index}"
        ));
        // SAFETY: C just produced this by-value slot in
        // `frankenphp_register_thread_for_kill`. No registry slot can retain
        // it, so release its platform resource through the matching C helper.
        unsafe { frankenphp_release_thread_for_kill(slot) };
        return;
    };

    thread.store_force_kill_slot(slot);
}

/// Blocks until the handler returns work or a lifecycle stop signal.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_before_script_execution(thread_index: usize) -> *mut c_char {
    let Some(thread) = thread_by_index(thread_index) else {
        write_diagnostic(format_args!(
            "before-script callback for unknown thread {thread_index}"
        ));
        return ptr::null_mut();
    };

    let Some(script_path) = thread.before_script_execution() else {
        return ptr::null_mut();
    };

    // `ScriptPath` is already non-empty and NUL-terminated. Once the handler
    // returned it, this callback cannot reject it: a real handler may already
    // have dequeued a request, and C guarantees the paired after-script call
    // only for a non-NULL return (`frankenphp.c:1506-1562`). The thread owns
    // this allocation until that paired callback releases it.
    thread.publish_script_path(script_path).cast::<c_char>()
}

/// Runs handler cleanup and releases the path borrowed by C.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_after_script_execution(thread_index: usize, exit_status: c_int) {
    let Some(thread) = thread_by_index(thread_index) else {
        write_diagnostic(format_args!(
            "after-script callback for unknown thread {thread_index}"
        ));
        return;
    };

    if exit_status < 0 {
        // Upstream raises ErrScriptExecution. Rust cannot unwind through this
        // C boundary, and cleanup must still run to complete the request.
        write_diagnostic(format_args!(
            "PHP script on thread {thread_index} returned negative status {exit_status}"
        ));
    }
    thread.after_script_execution(exit_status);
    thread.release_script_path();
}

/// Clears the slot before C frees its TSRM-backed `EG()` pointers.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_clear_force_kill_slot(thread_index: usize) {
    let Some(thread) = thread_by_index(thread_index) else {
        write_diagnostic(format_args!(
            "force-kill clear for unknown thread {thread_index}"
        ));
        return;
    };
    thread.clear_force_kill_slot();
}

/// Publishes the stable state reached by a healthy `php_thread()` exit.
#[unsafe(no_mangle)]
pub extern "C" fn go_frankenphp_on_thread_shutdown(thread_index: usize) {
    let Some(thread) = thread_by_index(thread_index) else {
        write_diagnostic(format_args!(
            "shutdown callback for unknown thread {thread_index}"
        ));
        return;
    };
    thread.on_thread_shutdown();
}

fn write_diagnostic(arguments: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "frankenrust: {arguments}");
}
