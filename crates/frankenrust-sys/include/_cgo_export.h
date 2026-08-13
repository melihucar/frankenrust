/* Hand-written replacement for the header cgo generates at build time.
 *
 * vendor/frankenphp/frankenphp.c:47 does `#include "_cgo_export.h"`, which
 * upstream's Go build produces automatically from every `//export go_*`
 * function in the package. That generation step does not exist here: we
 * compile frankenphp.c directly with `cc`, so this file supplies the same
 * declarations by hand. It must stay in lock-step with the 26 `go_*` symbols
 * frankenphp.c calls (see docs/PORTING-NOTES.md and issue #7) and with the
 * signatures those call sites actually use — verified against
 * `grep -on 'go_[a-z_]*(' vendor/frankenphp/frankenphp.c`.
 *
 * Do not edit vendor/frankenphp/frankenphp.c to avoid needing this file; it
 * is the read-only oracle (see AGENTS instructions / docs/PORTING-NOTES.md).
 *
 * Every symbol declared below is defined in Rust, in
 * crates/frankenrust-core/src/callbacks/*, as
 * `#[unsafe(no_mangle)] pub extern "C" fn`. This issue (#7) only wires them
 * up as abort-stubs; #10, #11, #12 and #14 give them real bodies.
 *
 * One exception, added by #11 (see issue #75): go_register_server_variables
 * is defined in ../shim.c, in C. Everything it calls allocates through the
 * Zend request allocator, whose out-of-memory path is zend_bailout() -- a
 * longjmp to a zend_catch above the callback. Go tolerates that jump crossing
 * a live cgo frame; Rust has no defined behaviour for it crossing a Rust
 * frame, so that callback's C-ABI entry point has to be C, and Rust supplies
 * only the half that touches no PHP API
 * (frankenrust_collect_server_vars, frankenrust_shim.h).
 *
 * Included by frankenphp.c after php.h, the Zend/TSRM/SAPI headers and
 * frankenphp.h (frankenphp.c:1-47), so `zend_string`, `zval`, `zend_llist`,
 * `zend_array`, `zend_long`, `sapi_request_info` and `go_string` are all
 * already visible by the time this file is parsed there. This header itself
 * includes frankenphp.h so it is also self-contained when bindgen parses it
 * on its own (see build.rs).
 */
#ifndef _CGO_EXPORT_H
#define _CGO_EXPORT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "frankenphp.h" /* go_string, force_kill_slot */

/* --- cgo multi-return structs, referenced by tag at frankenphp.c:765-766,
 * :851-852, :965 and :1140-1141. C requires these to exist as tagged structs
 * (not typedefs), because the call sites spell out `struct go_x_return`. */

struct go_apache_request_headers_return {
  go_string *r0;
  size_t r1;
};

struct go_frankenphp_worker_handle_request_start_return {
  bool r0;
  void *r1;
};

struct go_ub_write_return {
  size_t r0;
  bool r1;
};

struct go_mercure_publish_return {
  zend_string *r0;
  short r1;
};

/* --- the 26 go_* callbacks C calls into. Grouped and commented with the
 * frankenrust-core/src/callbacks/*.rs module that defines each one (issue
 * #7's pre-declared, frozen module layout) and the frankenphp.c call
 * site(s). */

/* callbacks/thread.rs */
void go_frankenphp_store_force_kill_slot(uintptr_t threadIndex,
                                         force_kill_slot slot); /* :299 */
char *go_frankenphp_before_script_execution(uintptr_t threadIndex); /* :1506 */
void go_frankenphp_after_script_execution(uintptr_t threadIndex,
                                          int exitStatus); /* :1562, :1591 */
void go_frankenphp_on_thread_shutdown(uintptr_t threadIndex); /* :1607 */
void go_frankenphp_clear_force_kill_slot(uintptr_t threadIndex); /* :1598 */

/* callbacks/output.rs */
struct go_ub_write_return go_ub_write(uintptr_t threadIndex, char *cBuf,
                                      size_t length); /* :1141 */
bool go_write_headers(uintptr_t threadIndex, int status,
                      zend_llist *headers); /* :1169 */
unsigned char go_sapi_flush(uintptr_t threadIndex); /* :1186 */

/* callbacks/input.rs */
size_t go_read_post(uintptr_t threadIndex, char *cBuf,
                    size_t countBytes); /* :1192 */
char *go_read_cookies(uintptr_t threadIndex); /* :1196 */
struct go_apache_request_headers_return
go_apache_request_headers(uintptr_t threadIndex); /* :766 */

/* callbacks/servervars.rs */
void go_register_server_variables(uintptr_t threadIndex,
                                  zval *trackVarsArray); /* :1379 */
char *go_update_request_info(uintptr_t threadIndex,
                             sapi_request_info *info); /* :355 */

/* callbacks/mainthread.rs */
void go_frankenphp_main_thread_is_ready(void); /* :1710 */
void go_frankenphp_shutdown_main_thread(void); /* :1727 */
char *go_get_custom_php_ini(bool disableTimeouts); /* :1681, :1685 */
void go_init_os_env(zend_array *mainThreadEnv); /* :1698 */

/* callbacks/worker.rs */
struct go_frankenphp_worker_handle_request_start_return
go_frankenphp_worker_handle_request_start(uintptr_t threadIndex); /* :852 */
void go_frankenphp_finish_worker_request(uintptr_t threadIndex,
                                         zval *retval); /* :911 */
void go_frankenphp_finish_php_request(uintptr_t threadIndex); /* :634 */

/* callbacks/log.rs */
void go_log(uintptr_t threadIndex, char *message, int level); /* :1386 */
char *go_log_attrs(uintptr_t threadIndex, zend_string *message,
                   zend_long cLevel, zval *cAttrs); /* :998, :1586 */

/* callbacks/misc.rs */
bool go_is_context_done(uintptr_t threadIndex); /* :627 */
bool go_putenv(char *name, int nameLen, char *val, int valLen); /* :682, :693 */
void go_schedule_opcache_reset(uintptr_t threadIndex); /* :1008 */
struct go_mercure_publish_return
go_mercure_publish(uintptr_t threadIndex, struct _zval_struct *topics,
                   zend_string *data, unsigned char private, zend_string *id,
                   zend_string *typ, unsigned long long retry); /* :965 */

#endif /* _CGO_EXPORT_H */
