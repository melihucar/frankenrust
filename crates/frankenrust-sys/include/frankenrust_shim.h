/* The C half of `go_register_server_variables` (issue #75).
 *
 * Hand-written, and deliberately NOT a modification of vendor/frankenphp/ --
 * that stays the read-only oracle. It has no upstream counterpart to port:
 * it exists only because Rust, unlike Go, has no defined behaviour for a
 * `longjmp` that crosses one of its stack frames, and `zend_bailout()` is a
 * `longjmp`. See shim.c for the full argument.
 *
 * The types below are the wire format between the Rust half (which decides
 * *what* to register and owns the memory) and the C half (which does the
 * registering, and is the only side that may be on the stack when Zend bails
 * out).
 */
#ifndef _FRANKENRUST_SHIM_H
#define _FRANKENRUST_SHIM_H

#include "frankenphp.h"

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* One `$_SERVER["HTTP_*"]` entry, already mangled and `", "`-joined by Rust
 * (`cgi.go:150-164`). Exactly one of the two key forms is used per entry. */
typedef struct frankenrust_header_var {
  /* Non-NULL for one of the ~101 pre-interned common header keys
   * (`internal/phpheaders/phpheaders.go:15-118`): register through
   * `frankenphp_register_known_variable`, which skips PHP's key
   * sanitisation. NULL selects `key` below instead. */
  zend_string *known_key;
  /* NUL-terminated "HTTP_..." key for every other header
   * (`phpheaders.go:126` appends the NUL for exactly this reason), for
   * `frankenphp_register_variable_safe`. Read only when `known_key` is
   * NULL. */
  char *key;
  char *value;
  size_t value_len;
} frankenrust_header_var;

/* Everything one `go_register_server_variables` call hands to PHP: the single
 * by-value bulk struct (`cgi.go:104`) plus the per-header array.
 *
 * Every pointer reachable from here -- including `vars`'s 16 `char *` fields
 * and `headers` itself -- borrows memory owned by the thread's
 * `RequestContext`, which outlives the whole call and is only reclaimed when
 * the context slot is cleared or replaced. Nothing here is freed by C. */
typedef struct frankenrust_server_vars_batch {
  frankenphp_server_vars vars;
  const frankenrust_header_var *headers;
  size_t num_headers;
} frankenrust_server_vars_batch;

/* Implemented in Rust: crates/frankenrust-core/src/callbacks/servervars.rs.
 *
 * Fills `*out` from `thread_index`'s `RequestContext` and returns true, or
 * returns false (leaving `*out` untouched) when there is nothing to register
 * -- no context, or a context with no request, which is upstream's
 * `if fc.request != nil` guard at `cgi.go:179`.
 *
 * Makes no call into PHP, so it cannot bail out. That is the whole point:
 * it must have *returned* before its caller makes the first Zend call. */
bool frankenrust_collect_server_vars(uintptr_t thread_index,
                                     frankenrust_server_vars_batch *out);

#endif
