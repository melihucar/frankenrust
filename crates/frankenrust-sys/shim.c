/* `go_register_server_variables`: the one `go_*` callback whose C-ABI entry
 * point is written in C rather than in frankenrust-core.
 *
 * Why this file exists
 * --------------------
 * `frankenphp_register_server_vars`, `frankenphp_register_known_variable` and
 * `frankenphp_register_variable_safe` all grow and populate `$_SERVER`'s
 * HashTable through the Zend *request* allocator. On `memory_limit`
 * exhaustion that allocator does not return an error: it calls
 * `zend_error_noreturn(E_ERROR, "Allowed memory size ... exhausted")`, which
 * ends in `zend_bailout()` -- a `longjmp` back to the nearest enclosing
 * `zend_try` (here, `php_request_startup`'s, around `php_hash_environment()`;
 * in worker mode `frankenphp.c:565`'s), skipping every stack frame in between
 * without running any cleanup.
 *
 * Upstream (Go) lives with that: `go_register_server_variables` is a
 * cgo-exported Go function nested under that `zend_try`, so the jump crosses
 * a live Go frame. Rust's position is stricter: a `longjmp` across a Rust
 * frame is undefined behaviour regardless of what that frame owns and
 * regardless of whether it has destructors to run. So the port cannot
 * inherit the exposure, and -- this is the part two earlier designs for this
 * issue got wrong -- it cannot be fixed by catching the bailout in C and
 * *re-raising* it from Rust either: the re-raise is a `longjmp` from a C
 * frame nested inside the Rust callback frame to a `setjmp` below it, which
 * crosses that Rust frame just the same. Dropping everything first removes
 * the leak, not the undefined behaviour.
 *
 * The fix is structural: keep Rust off the stack entirely for the part that
 * can bail out. `go_register_server_variables` is therefore defined here, in
 * C, and splits into two phases:
 *
 *   1. `frankenrust_collect_server_vars()` -- Rust. Reads the thread's
 *      `RequestContext` under its slot lock, computes every key and value,
 *      stores them in memory the context owns, and *returns*. It makes no
 *      call into PHP, so it cannot bail out.
 *   2. everything below -- pure C. By the time the first Zend call runs, the
 *      Rust frame is gone and the stack from `zend_bailout()` down to
 *      `php_request_startup`'s `zend_catch` is C the whole way. No `zend_try`
 *      of our own is needed or wanted: the bailout propagates exactly where
 *      upstream's does, and PHP's control flow and error reporting are
 *      bit-for-bit upstream's.
 *
 * Nothing here allocates or frees: every pointer in the batch belongs to the
 * `RequestContext`, whose lifetime already spans the request (it is reclaimed
 * when the context slot is cleared or replaced -- see
 * `crates/frankenrust-core/src/context.rs`). So a bailout out of any call
 * below leaks nothing, in C or in Rust.
 *
 * See https://github.com/melihucar/frankenrust/issues/11 and its retracted
 * designs (a `zend_try` trampoline per callback, and a C trampoline around
 * just this one that returns a `bool`).
 *
 * Threading: this runs on a PHP thread that has already executed
 * `ZEND_TSRMLS_CACHE_UPDATE()` (`frankenphp.c:1491`, `:1647`) -- the same
 * precondition `frankenphp.c` itself relies on, since on non-Windows it does
 * not define the cache either and reads `EG()`/`SG()` throughout.
 */

/* Mirrors the prefix vendor/frankenphp/frankenphp.c uses before it reaches
 * _cgo_export.h (frankenphp.c:1-47): SAPI.h for `sapi_request_info`, which
 * _cgo_export.h's go_update_request_info declaration names. */
#include "frankenphp.h"

#include <SAPI.h>
#include <php.h>

#include "_cgo_export.h"
#include "frankenrust_shim.h"

/* `frankenphp.c:1379`, inside `frankenphp_register_variables()`
 * (`sapi_module_struct.register_server_variables`). Port of
 * `go_register_server_variables` (`cgi.go:174-188`).
 *
 * The prepared-environment merge (`cgi.go:185-187`) is out of scope for issue
 * #11 -- `fc.env` is not part of its `RequestContext` -- so it has no
 * counterpart here yet. */
void go_register_server_variables(uintptr_t threadIndex,
                                  zval *trackVarsArray) {
  frankenrust_server_vars_batch batch;

  /* Phase 1: Rust. Returns before phase 2 makes its first Zend call, which is
   * what makes phase 2's bailout path Rust-free. */
  if (!frankenrust_collect_server_vars(threadIndex, &batch)) {
    return;
  }

  /* Phase 2: pure C. `cgi.go:104` -- one bulk call for the known variables. */
  frankenphp_register_server_vars(trackVarsArray, batch.vars);

  /* `addHeadersToServer` (`cgi.go:150-164`). */
  for (size_t i = 0; i < batch.num_headers; i++) {
    const frankenrust_header_var *header = &batch.headers[i];

    if (header->known_key != NULL) {
      frankenphp_register_known_variable(header->known_key, header->value,
                                         header->value_len, trackVarsArray);
    } else {
      frankenphp_register_variable_safe(header->key, header->value,
                                        header->value_len, trackVarsArray);
    }
  }
}
