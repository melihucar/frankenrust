/* Bailout trampolines for the PHP calls frankenrust-core makes from Rust
 * frames while a Zend request is live. See shim.c for the full argument and
 * https://github.com/melihucar/frankenrust/issues/75.
 *
 * Hand-written, and deliberately NOT a modification of vendor/frankenphp/ --
 * that stays the read-only oracle. These wrappers exist only because Rust,
 * unlike Go, treats a longjmp across its frames as undefined behaviour, so
 * they have no upstream counterpart to port.
 */
#ifndef _FRANKENRUST_SHIM_H
#define _FRANKENRUST_SHIM_H

#include "frankenphp.h"

#include <stdbool.h>

/* Each of the three wrappers calls the identically-named upstream function
 * unmodified, inside a zend_try/zend_catch established in *this* C frame.
 * Returns true when the call completed, false when it bailed out. A false
 * return means the Zend engine has already reported a fatal error and the
 * request is doomed: the caller must not make further PHP calls, and must
 * re-raise via frankenrust_bailout() once its own frames are unwound. */
bool frankenrust_try_register_server_vars(zval *track_vars_array,
                                          frankenphp_server_vars vars);
bool frankenrust_try_register_known_variable(zend_string *z_key, char *value,
                                             size_t val_len,
                                             zval *track_vars_array);
bool frankenrust_try_register_variable_safe(char *key, char *var,
                                            size_t val_len,
                                            zval *track_vars_array);

/* Re-raises a bailout one of the wrappers above swallowed, resuming the
 * unwind exactly where upstream's would have gone. Never returns. */
void frankenrust_bailout(void);

#endif
