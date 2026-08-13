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

// ...and the fields at their declared offsets, not merely the right total.
// This struct crosses to `go_frankenphp_store_force_kill_slot` BY VALUE
// (frankenphp.c:299), and #10 will dereference both pointers to interrupt a
// runaway script: reading `timed_out` where `vm_interrupt` lives sets the
// wrong `zend_atomic_bool` and the force-kill silently never fires.
const _: () = {
    use std::mem::offset_of;

    assert!(
        offset_of!(force_kill_slot, vm_interrupt) == 0,
        "force_kill_slot.vm_interrupt is no longer the first field (frankenphp.h:61-69)"
    );
    assert!(
        offset_of!(force_kill_slot, timed_out) == size_of::<usize>(),
        "force_kill_slot.timed_out is no longer the second field (frankenphp.h:61-69)"
    );
};

#[cfg(frankenphp_has_kill_signal)]
const _: () = assert!(
    std::mem::offset_of!(force_kill_slot, tid) == 2 * size_of::<usize>(),
    "force_kill_slot.tid is no longer the third field (frankenphp.h:64-65) -- pthread_t is \
     word-sized on every platform build.rs accepts, so anything else means the header changed"
);

// frankenphp.h:82-119, transcribed 1:1 in declaration order. NOTE: this
// struct has 36 fields (1 total_num_vars + 16 char*/size_t pairs + 3
// zend_string*), not the 40 issue #7's acceptance criteria states -- see
// the issue #7 handoff notes. Every field is still asserted here, so the
// discrepancy in that count does not weaken this check.
//
// Every field of this struct is word-sized on any target build.rs accepts --
// `size_t`, `char *` and `zend_string *` alike -- and C lays them out with no
// padding, so field *i* of the declaration sits at exactly
// `i * size_of::<usize>()`. Asserting that exact offset (rather than merely
// that the offsets increase) is what makes this check say what issue #7 asks
// it to say: the fields are where frankenphp.h puts them.
//
// It catches all three ways this can go wrong, each of which silently
// corrupts $_SERVER rather than failing:
//   * a dropped or renamed field -- `offset_of!` stops compiling outright;
//   * a transposed pair of same-typed fields (two `char *`s swapped keeps
//     every individual `offset_of!` call valid, so only the value catches it);
//   * a field whose type changed size, or padding appearing between them,
//     which shifts every later field by a constant nothing else would notice.
// The total-size assertion at the end additionally catches a field *appended*
// to the struct, which no per-field offset check can see.
const _: () = {
    use std::mem::offset_of;

    // Field n of a no-padding, all-word-sized struct sits at n * W.
    const W: usize = size_of::<usize>();

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

    let mut i = 0;
    while i < offsets.len() {
        assert!(
            offsets[i] == i * W,
            "frankenphp_server_vars does not match frankenphp.h:82-119 -- a field was \
             transposed, retyped, or padded away from its declared offset, which would \
             silently corrupt $_SERVER"
        );
        i += 1;
    }

    assert!(
        size_of::<frankenphp_server_vars>() == offsets.len() * W,
        "frankenphp_server_vars has grown or shrunk relative to frankenphp.h:82-119 -- a \
         field was added or removed and the offsets listed above never noticed"
    );
};
