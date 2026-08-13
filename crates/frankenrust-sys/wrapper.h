/* bindgen input. Mirrors the include prefix vendor/frankenphp/frankenphp.c
 * uses before it reaches frankenphp.h/_cgo_export.h (frankenphp.c:1-47), so
 * every type those two headers reference (zend_string, zval, zend_llist,
 * zend_array, sapi_request_info, ...) resolves the same way here as it does
 * in the real translation unit built by build.rs. Not compiled by `cc` --
 * only frankenphp.c and types.c are (see build.rs) -- this exists purely so
 * bindgen can see the same types.
 */
#include <SAPI.h>
#include <php.h>
#include <php_main.h>
#include <sapi/embed/php_embed.h>

#include "frankenphp.h"
#include "types.h"

#include "_cgo_export.h"
