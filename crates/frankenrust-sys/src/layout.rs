//! Compile-time layout guards for the two structs that cross the FFI
//! boundary by value (issue #7's acceptance criteria). A mismatch here
//! means bindgen parsed `frankenphp.h` under different conditions than the
//! `cc` invocation that actually compiled `frankenphp.c`, which would read
//! or write through the wrong offset -- silently, since both sides agree on
//! *a* layout, just not the same one.
//!
//! Every assertion below is a `const _: () = ...` block, not a `#[test]`:
//! it is checked once, unconditionally, whenever this crate is compiled at
//! all (by anything, including a downstream consumer that never runs `cargo
//! test`), and a violation is a build failure with the assertion's own
//! message, not a test failure someone has to notice.

use std::mem::size_of;

use crate::{force_kill_slot, frankenphp_server_vars};

// frankenphp.h:61-69:
//   typedef struct {
//     zend_atomic_bool *vm_interrupt;
//     zend_atomic_bool *timed_out;
//   #ifdef FRANKENPHP_HAS_KILL_SIGNAL
//     pthread_t tid;
//   #elif defined(PHP_WIN32)
//     HANDLE thread_handle;
//   #endif
//   } force_kill_slot;
//
// FRANKENPHP_HAS_KILL_SIGNAL is `!PHP_WIN32 && defined(SIGRTMIN)`
// (frankenphp.h:56-59) -- true on Linux, false on macOS (no realtime
// signals). build.rs's `probe_has_kill_signal` asks the real preprocessor
// for this exact condition (not `target_os`) and emits the matching
// `--cfg`, so the expected word count below is checked against the same
// condition the struct was actually compiled under, not a hardcoded number.
#[cfg(frankenphp_has_kill_signal)]
const FORCE_KILL_SLOT_WORDS: usize = 3; // vm_interrupt, timed_out, tid
#[cfg(not(frankenphp_has_kill_signal))]
const FORCE_KILL_SLOT_WORDS: usize = 2; // vm_interrupt, timed_out (no tid: no SIGRTMIN, not PHP_WIN32 either)

const _: () = assert!(
    size_of::<force_kill_slot>() == FORCE_KILL_SLOT_WORDS * size_of::<usize>(),
    "force_kill_slot's size no longer matches FRANKENPHP_HAS_KILL_SIGNAL (frankenphp.h:56-69) \
     -- did the header change, or did build.rs's probe_has_kill_signal desync from it?"
);

// frankenphp.h:82-119, transcribed 1:1 in declaration order. NOTE: this
// struct has 36 fields (1 total_num_vars + 16 char*/size_t pairs + 3
// zend_string*), not the 40 issue #7's acceptance criteria states -- see
// the issue #7 handoff notes. Every field is still asserted here, so the
// discrepancy in that count does not weaken this check.
//
// `offset_of!` fails to compile outright if bindgen ever drops or renames a
// field (a hard build error, not a silently-passing test); the monotonic
// order check below additionally catches a field silently transposed with
// an adjacent one of the same type, which `offset_of!` alone would not
// (swapping two `char *` fields keeps every individual `offset_of!` call
// valid).
const _: () = {
    use std::mem::offset_of;

    let offsets = [
        offset_of!(frankenphp_server_vars, total_num_vars),
        offset_of!(frankenphp_server_vars, remote_addr),
        offset_of!(frankenphp_server_vars, remote_addr_len),
        offset_of!(frankenphp_server_vars, remote_host),
        offset_of!(frankenphp_server_vars, remote_host_len),
        offset_of!(frankenphp_server_vars, remote_port),
        offset_of!(frankenphp_server_vars, remote_port_len),
        offset_of!(frankenphp_server_vars, document_root),
        offset_of!(frankenphp_server_vars, document_root_len),
        offset_of!(frankenphp_server_vars, path_info),
        offset_of!(frankenphp_server_vars, path_info_len),
        offset_of!(frankenphp_server_vars, php_self),
        offset_of!(frankenphp_server_vars, php_self_len),
        offset_of!(frankenphp_server_vars, document_uri),
        offset_of!(frankenphp_server_vars, document_uri_len),
        offset_of!(frankenphp_server_vars, script_filename),
        offset_of!(frankenphp_server_vars, script_filename_len),
        offset_of!(frankenphp_server_vars, script_name),
        offset_of!(frankenphp_server_vars, script_name_len),
        offset_of!(frankenphp_server_vars, server_name),
        offset_of!(frankenphp_server_vars, server_name_len),
        offset_of!(frankenphp_server_vars, server_port),
        offset_of!(frankenphp_server_vars, server_port_len),
        offset_of!(frankenphp_server_vars, content_length),
        offset_of!(frankenphp_server_vars, content_length_len),
        offset_of!(frankenphp_server_vars, server_protocol),
        offset_of!(frankenphp_server_vars, server_protocol_len),
        offset_of!(frankenphp_server_vars, http_host),
        offset_of!(frankenphp_server_vars, http_host_len),
        offset_of!(frankenphp_server_vars, request_uri),
        offset_of!(frankenphp_server_vars, request_uri_len),
        offset_of!(frankenphp_server_vars, ssl_cipher),
        offset_of!(frankenphp_server_vars, ssl_cipher_len),
        offset_of!(frankenphp_server_vars, request_scheme),
        offset_of!(frankenphp_server_vars, ssl_protocol),
        offset_of!(frankenphp_server_vars, https),
    ];

    let mut i = 1;
    while i < offsets.len() {
        assert!(
            offsets[i - 1] < offsets[i],
            "frankenphp_server_vars fields are out of declaration order relative to \
             frankenphp.h:82-119 -- a field was transposed, which would silently corrupt \
             $_SERVER"
        );
        i += 1;
    }
};
