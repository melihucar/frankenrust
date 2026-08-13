# PORTING-NOTES — Go/C → Rust

Read this before writing any code. It is the pattern map for this port; where it
disagrees with your instinct about idiomatic Rust, it wins, because consistency
across parallel agents matters more here than any individual file being pretty.

Upstream lives at `vendor/frankenphp/` and is **read-only**. It is the oracle.

## The single most important fact about this port

**FrankenPHP's threading model already lives in C, not Go.** There is no
`runtime.LockOSThread` anywhere in the upstream tree. PHP threads are real POSIX
threads created by `pthread_create` inside `frankenphp.c`, and the per-thread
loop is C:

```c
/* frankenphp.c — php_thread() */
#ifdef ZTS
  (void)ts_resource(0);          /* allocate this thread's TSRM storage */
#endif
  zend_first_try {
    char *scriptName = NULL;
    while ((scriptName = go_frankenphp_before_script_execution(thread_index))) {
      frankenphp_update_request_context();
      php_request_startup();
      zend_stream_init_filename(&file_handle, scriptName);
      php_execute_script(&file_handle);
      php_request_shutdown((void *)0);
      go_frankenphp_after_script_execution(thread_index, EG(exit_status));
    }
  } zend_catch { /* thread unhealthy -> restarted */ }
#ifdef ZTS
  ts_free_thread();
#endif
```

Consequences you must internalise:

- We **keep `frankenphp.c` essentially as-is.** We are not porting it. It is
  already language-neutral C that happens to call symbols named `go_*`.
- The port is therefore *not* "rewrite FrankenPHP." It is: **reimplement the 25
  functions that C calls back into, in Rust instead of Go**, plus the request
  plumbing behind them, plus an HTTP server to feed it.
- PHP never runs on a Rust async task. It runs on the pthread C created. Rust
  code called from `go_*` callbacks is executing **on that pthread**, and must
  not block on an async runtime, must not `.await`, and must not assume tokio
  context. Crossing that line is the defining bug class of this project.

## Prior art — read this before assuming you are first

You are not. Several people have built a Rust PHP app server already, and one
crate has done much of the unsafe groundwork:

- **[ext-php-rs](https://github.com/extphprs/ext-php-rs)** (v0.15.15, actively
  maintained) ships an opt-in `embed` feature containing a `Sapi` trait,
  `PhpThreadGuard` (an RAII wrapper around `ts_resource(0)` /
  `TSRMLS_CACHE_UPDATE`), and worker-mode lifecycle functions
  (`worker_request_startup` / `worker_request_shutdown` /
  `worker_reset_superglobals`). The PR that added it is literally described as
  *"Safe abstractions for building custom PHP SAPIs in Rust (FrankenPHP-style
  embedded PHP)."* **It is invisible on docs.rs** — that URL 404s, because
  docs.rs only builds default features. Read the GitHub source tree instead.
- **[Pasir](https://github.com/el7cosmos/pasir)** (~133 stars) is hyper + tokio
  + ext-php-rs's custom SAPI — essentially this project, already built. Its
  author also contributes to ext-php-rs's embed module. Self-described as
  experimental and not production-ready.
- **[Turbine](https://github.com/turbine-dev/turbine)** (~90 stars) and
  **[ePHPm](https://github.com/ephpm/ephpm)** (~32) are similar attempts.

**We still compile upstream's `frankenphp.c` rather than implementing
ext-php-rs's `Sapi` trait, and the reason is the benchmark, not pride.** The
whole question this project asks is "what does the host language cost?" If we
write our own SAPI, we change the SAPI *and* the language at once, and any
measured difference is unattributable. Keeping upstream's exact C SAPI holds
that variable fixed so the delta means something.

Use ext-php-rs as a **reference for the hard parts** — its `PhpThreadGuard`, its
`build.rs`, and its `allowed_bindings.rs` are worked solutions to problems you
are about to hit. Do not take it as a dependency for the SAPI itself.

## The FFI surface — this is the checklist

Upstream's C calls exactly these symbols. Each becomes a Rust
`#[unsafe(no_mangle)] pub extern "C" fn`. Names are kept **identical** so
`frankenphp.c` needs no edits.

| Symbol | Upstream Go | What it does |
|---|---|---|
| `go_ub_write` | `frankenphp.go:430` | PHP output → response writer |
| `go_write_headers` | `frankenphp.go:572` | send response headers |
| `go_sapi_flush` | `frankenphp.go:624` | flush; detect aborted connection |
| `go_read_post` | `frankenphp.go:662` | read request body |
| `go_read_cookies` | `frankenphp.go:703` | `Cookie` header |
| `go_apache_request_headers` | `frankenphp.go:490` | `getallheaders()` |
| `go_log` / `go_log_attrs` | `frankenphp.go:741/769` | error log, structured log |
| `go_is_context_done` | `frankenphp.go:804` | request cancellation |
| `go_register_server_variables` | `cgi.go:174` | bulk `$_SERVER` import |
| `go_update_request_info` | `cgi.go:284` | fills `sapi_request_info` |
| `go_frankenphp_before_script_execution` | `phpthread.go:234` | **hands a thread its next script** |
| `go_frankenphp_after_script_execution` | `phpthread.go:248` | post-script cleanup |
| `go_frankenphp_worker_handle_request_start` | `threadworker.go:272` | worker parks here for next request |
| `go_frankenphp_finish_worker_request` | `threadworker.go:297` | worker request done |
| `go_frankenphp_finish_php_request` | `threadworker.go:331` | `fastcgi_finish_request()` |
| `go_frankenphp_main_thread_is_ready` | `phpmainthread.go:248` | main thread ready/park |
| `go_frankenphp_shutdown_main_thread` | `phpmainthread.go:280` | main thread teardown |
| `go_get_custom_php_ini` | `phpmainthread.go:288` | php.ini overrides |
| `go_frankenphp_on_thread_shutdown` | `phpthread.go:283` | thread exited C loop |
| `go_frankenphp_store_force_kill_slot` | `phpthread.go:260` | publish `EG(vm_interrupt)` ptr |
| `go_frankenphp_clear_force_kill_slot` | `phpthread.go:271` | clear before `ts_free_thread()` |
| `go_init_os_env` / `go_putenv` | `env.go:13/26` | env sandboxing |
| `go_schedule_opcache_reset` | `frankenphp.go:809` | opcache reset → thread reboot |
| `go_mercure_publish` | `mercure.go:20` | **out of scope**: return a stub |

## Construct mapping

| Go / cgo | Rust | Notes |
|---|---|---|
| `//export go_foo` | `#[unsafe(no_mangle)] pub extern "C" fn go_foo` | name must match exactly |
| `*C.char` in | `unsafe { CStr::from_ptr(p) }` | PHP strings are **arbitrary bytes**, not UTF-8. Use `.to_bytes()`, never `.to_str().unwrap()` |
| `C.CString(s)` | `libc::malloc` + `ptr::copy_nonoverlapping` + trailing NUL (a named helper that checks the `malloc` result for NULL) | ownership crosses to C; C frees it with libc `free()`. `CString::into_raw` allocates from Rust's **global allocator**, not libc's `malloc` — handing that pointer to C's `free()` is UB; `into_raw` is correct only for pointers that come back to Rust via `CString::from_raw`. Interior-NUL handling is per callback, not one policy — e.g. `go_read_cookies` strips NULs (`frankenphp.go:716`) where `CString::new` would instead error, so follow the callback's own rule |
| `C.GoString` / `C.GoBytes` | `slice::from_raw_parts(p, len).to_vec()` | copy at the boundary; do not hand out borrows into PHP memory |
| `unsafe.Pointer` thread handle | `usize` thread index into a `Vec<ThreadSlot>` | never a raw `*mut` into a Rust collection that may reallocate |
| goroutine + `chan` | OS thread + `crossbeam_channel` / `std::sync::mpsc` | **not** tokio channels on the PHP side |
| `select { case <-a: case <-b: }` | `crossbeam_channel::select!` | upstream's `waitForWorkerRequest` is exactly this |
| `context.Context` cancel | `Arc<AtomicBool>` + a `Notify` on the async side | `go_is_context_done` reads the bool from the pthread |
| `sync.Mutex` | `parking_lot::Mutex` | avoid holding across an FFI call into PHP |
| `atomic.Int32` state machine | `AtomicU8` + explicit `Ordering` | mirror `internal/state/state.go` states exactly, same names |
| `net/http` `ServeHTTP` | `hyper` service fn | the async boundary; hands off to the PHP thread pool and awaits a oneshot |

## The async ↔ pthread boundary

This is the one piece of real architecture we are adding, because Go blurred it
and Rust will not let us.

```
hyper (tokio)                     PHP pthread (created by C)
─────────────                     ──────────────────────────
service_fn(req)
  build RequestCtx
  send on crossbeam channel  ───► go_frankenphp_before_script_execution
  await oneshot::Receiver           (blocks in C until a request arrives)
                                  php_execute_script
                                    go_ub_write ──► pushes bytes into RequestCtx
                                  go_frankenphp_after_script_execution
  ◄─── oneshot::Sender.send()        signals completion
  return Response
```

Rules:
- The tokio side **never** touches PHP state. It owns the socket and the
  `RequestCtx`; the pthread side borrows the `RequestCtx` for the duration of
  one script run and signals a oneshot when done.
- Never `spawn_blocking` for PHP work. `spawn_blocking` threads are pooled and
  recycled by tokio; PHP threads are created by C, live for many requests, and
  own TSRM storage that must not migrate. These are incompatible lifecycles.
- Response bytes cross via a channel or a `Mutex<BytesMut>`, not by handing the
  pthread a `&mut` into something tokio also holds.

## Rules about `unsafe`

Every `unsafe` block carries a `// SAFETY:` comment naming the invariant and
where it is established. The recurring ones:

- *"Called only from `php_thread()` on the thread that owns `thread_index`."*
- *"`zval` is valid for the duration of this call; we copy before returning."*
- *"Pointer was `libc::malloc`'d in `go_x` and is freed by C's `free()` at `frankenphp.c:NNN`."*

Bun's rewrite shipped 13,365 `unsafe` blocks and Zig's creator called the result
"unreviewed slop." That criticism lands hardest exactly where SAFETY comments
are absent. We have a two-reviewer gate specifically to catch this; help it.

## Out of scope for the thin slice

Do not implement, and do not stub in a way that pretends otherwise: TLS, HTTP/2,
HTTP/3, static file serving, Caddyfile parsing, the admin API, Mercure, Vulcain,
metrics, the file watcher, `internal/extgen`, autoscaling. Where upstream calls
into these, return a clearly-named unimplemented result and log once.
