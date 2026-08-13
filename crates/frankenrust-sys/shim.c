/* Bailout trampolines: the C frames that own the `setjmp` for every PHP call
 * frankenrust-core makes while a Zend request is live.
 *
 * Why this file exists
 * --------------------
 * `frankenphp_register_server_vars`, `frankenphp_register_known_variable` and
 * `frankenphp_register_variable_safe` all grow and populate `$_SERVER`'s
 * HashTable through the Zend *request* allocator. On `memory_limit`
 * exhaustion that allocator does not return an error: it calls
 * `zend_error_noreturn(E_ERROR, "Allowed memory size ... exhausted")`, which
 * ends in `zend_bailout()` -- a `longjmp` back to the nearest enclosing
 * `zend_try`, skipping every stack frame in between without running any
 * cleanup.
 *
 * Upstream (Go) lives with that: `go_register_server_variables` is a
 * cgo-exported Go function nested under `php_request_startup`'s own
 * `zend_try`, so the jump crosses a live Go frame. Rust's position is
 * stricter -- unwinding across its frames by any mechanism other than its own
 * panic machinery is undefined behaviour regardless of what those frames own
 * -- so the port cannot simply inherit the exposure.
 *
 * The fix is to move the `setjmp` *below* the Rust frames. Each wrapper here
 * establishes its own `zend_try` inside a pure C frame, calls the upstream
 * function unmodified, and converts a bailout into an ordinary `false`
 * return. frankenrust-core then unwinds its own frames by ordinary Rust
 * returns -- running every destructor -- and, once nothing but the
 * `extern "C"` callback frame is left, calls `frankenrust_bailout()` to
 * resume the unwind exactly where upstream's would have gone.
 *
 * Net effect: PHP's control flow and error reporting are bit-for-bit
 * upstream's (the fatal error is already reported by the time we catch; the
 * bailout still lands in `php_request_startup`'s `zend_catch`, which returns
 * FAILURE, which `frankenphp.c:1512-1515` turns back into a bailout to
 * `frankenphp_php_thread`'s `zend_first_try` at `:1504`), while no Rust frame
 * holding a destructor is ever between the `setjmp` and the `longjmp`.
 *
 * See https://github.com/melihucar/frankenrust/issues/75.
 *
 * Threading: `zend_try` reads and writes `EG(bailout)`, which in a ZTS build
 * resolves through the `_tsrm_ls_cache` TLS slot. Every function here is
 * therefore callable only from a PHP thread that has already run
 * `ZEND_TSRMLS_CACHE_UPDATE()` (`frankenphp.c:1491`, `:1647`) -- the same
 * precondition `frankenphp.c` itself relies on, since on non-Windows it does
 * not define the cache either and reads `EG()` throughout.
 */

#include "frankenphp.h"

#include <php.h>

#include "frankenrust_shim.h"

/* `ok` is written inside `zend_catch`, i.e. after a `longjmp` has re-entered
 * this frame, and read after `zend_end_try()`. `volatile` is what C requires
 * for a local whose value must survive that path (and is the idiom PHP's own
 * sources use around these macros). */

bool frankenrust_try_register_server_vars(zval *track_vars_array,
                                          frankenphp_server_vars vars) {
  volatile bool ok = true;

  zend_try { frankenphp_register_server_vars(track_vars_array, vars); }
  zend_catch { ok = false; }
  zend_end_try();

  return ok;
}

bool frankenrust_try_register_known_variable(zend_string *z_key, char *value,
                                             size_t val_len,
                                             zval *track_vars_array) {
  volatile bool ok = true;

  zend_try {
    frankenphp_register_known_variable(z_key, value, val_len, track_vars_array);
  }
  zend_catch { ok = false; }
  zend_end_try();

  return ok;
}

bool frankenrust_try_register_variable_safe(char *key, char *var,
                                            size_t val_len,
                                            zval *track_vars_array) {
  volatile bool ok = true;

  zend_try {
    frankenphp_register_variable_safe(key, var, val_len, track_vars_array);
  }
  zend_catch { ok = false; }
  zend_end_try();

  return ok;
}

/* Only ever called after one of the wrappers above returned false, so
 * `EG(bailout)` is guaranteed non-NULL here: the wrapper's `zend_end_try()`
 * restored the enclosing bailout address that the engine was heading for
 * before we intercepted it. */
void frankenrust_bailout(void) { zend_bailout(); }
