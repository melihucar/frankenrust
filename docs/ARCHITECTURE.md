# ARCHITECTURE — FrankenRust

This is the **design** document: what the pieces are, how they fit, and why the
shape is what it is. For the Go→Rust construct table, the FFI checklist, and
the `unsafe` rules, read `docs/PORTING-NOTES.md` — this file links to it
instead of repeating it.

Every claim about upstream behaviour below cites `vendor/frankenphp/<file>:<line>`.
If you're reading this to understand a piece of upstream, follow the citation
and read the real code; this document is a map, not a replacement for the
territory.

## Why `frankenphp.c` is compiled, not ported

Upstream is three layers of very different portability (`README.md:15-19`):

| Layer | Upstream | Here |
|---|---|---|
| PHP SAPI glue (`frankenphp.c`, 1,961 lines of C) | C | kept, compiled as-is |
| Thread orchestration, request plumbing (~5,300 lines Go) | Go | ported to Rust |
| HTTP server, TLS, HTTP/3, config | Caddy | hyper, HTTP/1.1 only |

We keep `frankenphp.c` rather than reimplementing PHP embedding in Rust. This
is a benchmark decision, not a taste one: the question this project asks is
"what does the host language cost?" If we wrote our own SAPI, we would be
changing the SAPI *and* the host language in the same measurement, and any
observed difference would be unattributable to either one
(`docs/PORTING-NOTES.md:70-75`). The C is already language-neutral — it just
calls symbols that happen to be named `go_*` (`README.md:21-24`,
`docs/PORTING-NOTES.md:39-40`) — so holding it fixed and reimplementing only
what calls into it isolates the variable the benchmark exists to measure.

Concretely, "port FrankenPHP" becomes: **reimplement the callback functions C
calls into, in Rust instead of Go, plus the request plumbing behind them, plus
an HTTP server to feed it** (`docs/PORTING-NOTES.md:41-43`). That symbol list —
26 functions — is the porting checklist. It is not repeated here because it is a
checklist, not a design decision, but note that it exists in **two forms, in two
places, and they carry different columns**:

- `docs/PORTING-NOTES.md:81-112` lists each symbol against its **Go definition
  site** and a one-line description of what it does — the header is
  `| Symbol | Upstream Go | What it does |` (`docs/PORTING-NOTES.md:87`), so
  `frankenphp.go:430` there means "where `go_ub_write` is written in Go", not
  where C calls it. Use this to find the upstream implementation to port.
- Issue #7's body carries the other half: each symbol's **C signature and the
  `frankenphp.c` line that calls it** (e.g. `go_ub_write` at `1141`,
  `go_frankenphp_after_script_execution` at `1562, 1591`). Use this when you
  need the caller — in particular when writing a `// SAFETY:` comment that names
  which C site invokes a callback and on which thread. Do not read a Go line
  number out of `PORTING-NOTES.md` and write it down as a C call site.

(One of the 26, `go_mercure_publish`
(`docs/PORTING-NOTES.md:112`), is explicitly a stub, never a real
reimplementation — see "What is deliberately out of scope" below — which is
why `docs/PORTING-NOTES.md:41` and `README.md:165` count 25: this document
counts the checklist as written, stub included.)

## The three-layer split

The crate boundaries mirror the table above and are fixed by issue #7's
pre-declared module layout, which later issues are scoped against so that
parallel agents do not collide on the same files:

```
crates/frankenrust-sys      raw FFI: bindgen over PHP headers, compiles upstream's C
crates/frankenrust-core     safe layer: threads, request context, the 25 callbacks
crates/frankenrust-server   hyper HTTP/1.1 server, async<->pthread bridge
vendor/frankenphp/          upstream, READ-ONLY — the behavioural oracle
```

(`README.md:161-170`.)

- **`frankenrust-sys`** owns the unsafe FFI surface: bindgen-generated types
  from PHP's headers, a hand-written `_cgo_export.h` replacement (cgo
  normally generates this file at build time; upstream's C includes it at
  `frankenphp.c:47` and it does not exist in this tree), and `cc`-driven
  compilation of `vendor/frankenphp/frankenphp.c` and `types.c` unmodified.
  It exposes raw, `unsafe extern "C"` bindings and nothing else — no policy,
  no request logic.
- **`frankenrust-core`** owns everything that runs *on* a PHP pthread: the
  callback implementations (`crates/frankenrust-core/src/callbacks/*`, grouped
  by functional area rather than 1:1 with an upstream Go file — issue #7 fixes
  `output.rs`, `input.rs`, `servervars.rs`, `thread.rs`, `mainthread.rs`,
  `worker.rs`, `log.rs`, `misc.rs`; `mainthread.rs` and `misc.rs` each pull in
  callbacks from more than one upstream Go file, e.g. `misc.rs` takes
  `go_is_context_done`/`go_schedule_opcache_reset` from `frankenphp.go`,
  `go_putenv` from `env.go`, and `go_mercure_publish` from `mercure.go`), the
  thread state machine (`state.rs`), the per-thread lifecycle (`thread.rs`),
  and the request context (`context.rs`). This is the safe layer in the sense
  that it is where FFI unsafety gets wrapped and documented, not in the sense
  that it is free of `unsafe` — it is where nearly all of it lives.
- **`frankenrust-server`** owns the async side: the hyper HTTP/1.1 listener
  and the bridge that hands a request to a PHP thread and awaits its result
  without ever touching PHP state directly.

## The threading model

PHP threads are real POSIX threads created by `pthread_create` inside
`frankenphp.c` (`frankenphp_new_php_thread` / `frankenphp_new_main_thread`),
not tokio tasks, and the per-thread loop is C. `php_thread()`
(`vendor/frankenphp/frankenphp.c:1471-1619`) is the whole lifecycle of a PHP
OS thread:

```c
static void *php_thread(void *arg) {
  thread_index = (uintptr_t)arg;
  ...
#ifdef ZTS
  (void)ts_resource(0);              /* frankenphp.c:1489 */
#endif
  ...
  zend_first_try {
    char *scriptName = NULL;
    while ((scriptName = go_frankenphp_before_script_execution(thread_index))) {
      ...
      php_execute_script(&file_handle);   /* frankenphp.c:1531 */
      ...
      go_frankenphp_after_script_execution(thread_index, EG(exit_status));
    }
  }
  zend_catch { ... }
  zend_end_try();
  ...
#ifdef ZTS
  ts_free_thread();                  /* frankenphp.c:1602 */
#endif
  ...
}
```

`ts_resource(0)` allocates this thread's TSRM storage once, at the top; the
`while` loop runs any number of scripts on that same storage; `ts_free_thread()`
releases it only when the loop exits. A PHP interpreter is therefore bound to
one OS thread for its entire life — TSRM storage is per-thread and never
migrates.

Two callbacks in that loop are not notifications, they are the **scheduler**:

- **`go_frankenphp_main_thread_is_ready`** (called at `frankenphp.c:1710`,
  inside `php_main()`) must block for as long as the interpreter is meant to
  exist. C's comment at the call site is explicit about what returning means:
  "channel closed, shutdown gracefully... SAPI/TSRM teardown here is safe"
  (`frankenphp.c:1712-1714`) — returning immediately runs
  `frankenphp_sapi_module.shutdown()`, `sapi_shutdown()`, and `tsrm_shutdown()`
  (`frankenphp.c:1715-1720`). There are **two** intended reasons to return, not
  one: upstream parks on `WaitFor(state.Done, state.Rebooting)`
  (`vendor/frankenphp/phpmainthread.go:248-257`). `Done` is final shutdown.
  `Rebooting` is a *coordinated main-thread reboot*: the tear-down above is the
  point of it, and once C finishes it and calls `go_frankenphp_shutdown_main_thread`,
  Go flips `Rebooting → YieldingForReboot` (`phpmainthread.go:280-286`), which
  releases the reboot driver to call `frankenphp_new_main_thread` and rebuild
  the interpreter from scratch (`phpmainthread.go:203-208`). An implementation
  that parks until final shutdown only — the naive reading of "block for the
  server's lifetime" — deadlocks a reboot: the driver waits forever for
  `YieldingForReboot` that can never be published.
- **`go_frankenphp_before_script_execution`** (called at `frankenphp.c:1506`)
  is the `while` loop's condition: it blocks until a script is ready to run and
  returns the script name, or returns `NULL` to terminate the thread's loop.
  Note that only `NULL` terminates it — C tests the pointer and nothing else
  (`frankenphp.c:1506`), so a non-NULL pointer to an empty string enters
  request startup and tries to execute an empty filename. Upstream's handlers
  signal "stop" by returning the Go empty string, and the exported callback is
  what converts that to `NULL` before C ever sees it
  (`vendor/frankenphp/phpthread.go:234-246`); a Rust port needs the same
  narrowing at the same place. Upstream's blocking implementation
  (`regularThread.waitForRequest`, `vendor/frankenphp/threadregular.go:91-116`)
  parks on a `select` over its request channels and a drain channel until work
  or shutdown arrives — this is not a poll, it's a blocking rendezvous. Ours
  must behave the same way: no busy-waiting on the pthread.

## The async ↔ pthread boundary

This is the one piece of real architecture this port adds — Go blurred this
line with goroutines and cgo, and Rust will not let us. Full detail, including
the ASCII sequence diagram and the channel choice, is in
`docs/PORTING-NOTES.md:130-157`; the summary:

- The tokio side owns the socket and the request/response buffers. It never
  touches PHP state.
- The pthread side owns PHP execution. It holds the request state for the whole
  script run, streams bytes out through the output callback (`go_ub_write`),
  and signals completion back to the async side over a channel (a oneshot, per
  `docs/PORTING-NOTES.md:145`).
- **Completing the response and releasing the request are two different events,
  and the release is always the later one.** PHP exposes
  `frankenphp_finish_request()` to userland — with `fastcgi_finish_request()` as
  an alias (`vendor/frankenphp/frankenphp_arginfo.h:58`) — and it ends the
  response *mid-script*: it flushes output and headers and then calls
  `go_frankenphp_finish_php_request` (`frankenphp.c:623-637`, the call at
  `:634`), whose upstream implementation is just `fc.closeContext()`
  (`vendor/frankenphp/threadworker.go:331-341`).
  `closeContext` is what "sends the response to the client" — it closes
  `fc.done`, which is what releases the waiting HTTP handler
  (`vendor/frankenphp/context.go:135-147`). The script then keeps running on the
  same thread with its request state still live: `go_ub_write` dereferences the
  context on every subsequent write and only checks `fc.isDone` to decide to
  discard the bytes (`vendor/frankenphp/frankenphp.go:430-453`), and upstream
  pins the behaviour with a fixture whose script deliberately writes and logs
  after finishing (`vendor/frankenphp/testdata/finish-request.php:5-15`).
  So our oneshot can fire long before the pthread is done with the request. The
  request state must be *owned* by the PHP side until teardown, not borrowed
  from the async task for exactly as long as that task is awaiting; a design
  that drops the request when the oneshot resolves frees memory the interpreter
  still reads.
- **Where teardown lands differs by handler, and the diagram in `PORTING-NOTES`
  only draws the regular case.** For a regular thread it is script end:
  `afterScriptExecution` calls `afterRequest`, which calls `closeContext` — a
  no-op if the script already finished the request, since `closeContext` returns
  early on `fc.isDone` (`vendor/frankenphp/context.go:137-139`) — and then
  clears the thread's context pointer under `contextMu`
  (`vendor/frankenphp/threadregular.go:72-75`, `:129-135`), driven from C by
  `go_frankenphp_after_script_execution` (`frankenphp.c:1562`, or `:1591` on the
  bailout path), which C reaches after `php_execute_script`
  (`frankenphp.c:1531`) and `frankenphp_free_request_context()`
  (`frankenphp.c:1561`, or `:1590` on the bailout path). For a worker thread
  neither event lands there: the worker script is executed once and stays alive
  across many requests, calling `frankenphp_handle_request()`
  (`frankenphp.c:830`) in a `do ... while` loop — see any of upstream's worker
  fixtures, e.g. `vendor/frankenphp/testdata/worker-getopt.php:3-19`. Each
  request is installed by
  `go_frankenphp_worker_handle_request_start` (`frankenphp.c:851-852`) and both
  completed and released by `go_frankenphp_finish_worker_request`
  (`frankenphp.c:911`), which closes the context and nils the worker's context
  fields (`vendor/frankenphp/threadworker.go:298-327`); `afterScriptExecution`
  runs only when the long-lived worker script finally exits and the thread tears
  it down (`vendor/frankenphp/threadworker.go:78-80`). Completing the
  per-request oneshot at script end in worker mode would hang every worker
  request.
- **`spawn_blocking` is the wrong tool here.** Tokio's blocking pool is
  designed to be resized and recycled under load and to tear down idle
  threads; a PHP thread is created once by C, lives for many requests, and
  owns TSRM storage that must not migrate to a different OS thread mid-life.
  Handing PHP work to `spawn_blocking` would let tokio kill or reassign the
  very thread the interpreter's state is pinned to.
- The corollary is that there is **no Rust-side thread to create either**.
  `frankenphp_new_php_thread` calls `pthread_create` itself
  (`frankenphp.c:1743-1749`), and our callbacks are already running on that
  pthread when C invokes them. Spawning a `std::thread` and forwarding the work
  to it is the same bug as `spawn_blocking` wearing a different hat: it moves
  access to TSRM-backed interpreter state onto a thread that never called
  `ts_resource(0)` and has no PHP state of its own. The Rust side supplies
  *work* to threads C already owns, over a channel; it does not own the threads.

## The thread state machine and the handler indirection

The state machine each thread runs on is `vendor/frankenphp/internal/state/state.go`,
ported 1:1 by issue #8 into `crates/frankenrust-core/src/state.rs` — 14 states
(`Reserved`, `Booting`, `BootRequested`, `ShuttingDown`, `Done`, `Inactive`,
`Ready`, `TransitionRequested`, `TransitionInProgress`, `TransitionComplete`,
`Rebooting`, `ForceRebooting`, `RebootReady`, `YieldingForReboot`,
`state.go:13-37`), with the same subscribe-under-lock, release-before-block,
one-shot-broadcast semantics upstream relies on. This document does not
restate that design; #8's issue body is the authoritative spec for it.

What belongs here is *why* the state machine exists: a `phpThread` does not
always mean the same thing. What a thread does with the script name it gets
back from `go_frankenphp_before_script_execution` depends on which **handler**
is currently attached to it. Upstream models this as an interface,
`threadHandler` (`vendor/frankenphp/phpthread.go:38-50`), with three
implementations selected at runtime:

- **`inactiveThread`** (`vendor/frankenphp/threadinactive.go`) — a thread with
  no work assigned. Its `beforeScriptExecution` just waits for a state
  transition (`threadinactive.go:21-49`); ~350KB of memory held per idle
  thread, upstream notes, in exchange for a fast transition into real work
  (`threadinactive.go:11-12`).
- **`regularThread`** (`vendor/frankenphp/threadregular.go`) — executes PHP
  scripts in a web context, one request per loop iteration, parked in
  `waitForRequest` on a channel `select` between real work and shutdown
  (`threadregular.go:91-116`) between requests.
- **worker threads** (`vendor/frankenphp/threadworker.go`, driven by
  `go_frankenphp_worker_handle_request_start` /
  `go_frankenphp_finish_worker_request`) — run a long-lived PHP worker script
  that handles many requests itself without re-executing from scratch each
  time.

The indirection matters architecturally because it is what `setHandler` /
`transitionToNewHandler` (`vendor/frankenphp/phpthread.go:151-178`) exists to
change safely at runtime. The two halves run on different threads: `setHandler`
is called from *outside* the PHP thread, `transitionToNewHandler` runs *on* it,
and they meet in the middle at `(Ready or Inactive) → TransitionRequested →
TransitionInProgress → TransitionComplete` — `RequestSafeStateChange` only
starts the sequence from one of those two stable states, retrying once the
thread reaches one otherwise (`vendor/frankenphp/internal/state/state.go:199-222`).
Every handler *swap* is written inside that window, so no thread can observe one
mid-script. The one write that does not go through the window is the initial
assignment: `boot()` sets the handler to an `inactiveThread` under `handlerMu`
(`phpthread.go:68-71`) *before* it calls `frankenphp_new_php_thread`
(`phpthread.go:74`), so there is no PHP thread in existence yet to observe it.
That ordering is itself the invariant — a thread never starts without a handler,
and after it starts the handler only changes through the transition.

Note what upstream does **not** do here: it does not put the handler and the
state machine behind one lock. `phpThread` carries a `handlerMu` of its own
(`phpthread.go:24`) and `ThreadState` carries a separate `mu`
(`vendor/frankenphp/internal/state/state.go:74-77`). That separation is
load-bearing, not incidental — `setHandler` holds `handlerMu` for its whole body,
*including* across `state.WaitFor(state.TransitionInProgress)`
(`phpthread.go:153-168`), and the only thing that can publish
`TransitionInProgress` is the PHP thread itself in `transitionToNewHandler`
(`phpthread.go:172-178`). If both used the same primitive, the waiter would be
holding the lock the publisher needs, and every handler transition would
deadlock. So the property a Rust design must preserve is **ordering, not lock
identity**: the handler may only be mutated in the window the state machine
opens, and the lock guarding the handler must not be the lock the state
handshake travels through. The concrete Rust design for this (trait vs. enum, where
`thread.rs` draws the line) is not yet decided; issue #10 is where that gets
built, and this document deliberately does not prescribe it. Issue #8 covers
the state machine's own semantics only; the handler indirection above is
upstream context for whoever picks up #10, not something #8 implements.

## Ownership across the FFI boundary

There are at least four distinct string/memory ownership schemes live
simultaneously in upstream's C↔Go boundary. Confusing any two of them is UB.
Each Rust callback must document, in its `// SAFETY:` comment, which of these
schemes its arguments and return value follow — "the C code does this too" is
not an invariant (`docs/PORTING-NOTES.md:159-166`).

1. **Borrowed for the duration of one call; the callee copies immediately.**
   `frankenphp_ub_write` (`frankenphp.c:1133-1147`) calls `go_ub_write` with a
   `const char *str` that PHP owns and reuses after the call returns
   (`frankenphp.c:1140-1141`). The callback must copy out of it before
   returning and must never retain the pointer.
2. **Borrowed for the duration of one request; the caller pinned it.**
   Upstream's `phpThread` embeds a `runtime.Pinner` (`phpthread.go:19-20`) and
   uses `pinString`/`pinCString` (`phpthread.go:212-228`) to hand C a pointer
   into Go-owned memory without copying, valid until the request is unpinned.
   `frankenphp_free_request_context` documents the pairing directly: several
   `sapi_request_info` fields are "freed via thread.Unpin()"
   (`frankenphp.c:367-372`), i.e. not freed by C at all — ownership stays on
   the Go/Rust side for the request's lifetime. This is the scheme that needs
   an arena on our side: something that outlives one call but is reclaimed
   deterministically, not by relying on a GC-adjacent pinner. Note that
   `Unpin` is a *bulk* release — the `Pinner` is embedded in `phpThread`
   (`phpthread.go:20`), so `thread.Unpin()` drops everything that thread pinned,
   not one value. That is arena semantics already, which is why the question
   below is "where is the single reclaim point" rather than "who drops what".

   **The reclaim point differs by handler, exactly as teardown does, and getting
   this wrong is a leak rather than a crash — so no test will catch it.**

   - *Regular threads*: `thread.Unpin()` runs at the end of
     `go_frankenphp_after_script_execution` (`phpthread.go:248-258`, the call at
     `:257`), which C reaches at `frankenphp.c:1562` — after
     `frankenphp_free_request_context()` at `:1561` (bailout path: `:1590`,
     `:1591`). That is script end, not response end — see the async ↔ pthread
     boundary above: a script that called `frankenphp_finish_request()` has
     already sent its response and is still reading this memory.
   - *Worker threads*: `after_script_execution` is **not** a per-request event.
     A worker's `afterScriptExecution` is `tearDownWorkerScript`
     (`threadworker.go:78-80`), which runs only when the long-lived worker script
     finally exits. The pins, however, are re-created on *every* request:
     `frankenphp_worker_request_startup` calls `frankenphp_update_request_context()`
     (`frankenphp.c:563`), which calls `go_update_request_info`
     (`frankenphp.c:354-355`), which pins `query_string`, `content_type`,
     `path_translated`, `request_uri`, the `Authorization` header it returns, and
     `request_method` when it is not one of the cached constants
     (`cgi.go:298-323`). Upstream therefore reclaims at the **top of the next
     request** instead: `waitForWorkerRequest` opens with
     `handler.thread.Unpin()` under the comment "unpin any memory left over from
     previous requests" (`threadworker.go:199-201`), reached from
     `go_frankenphp_worker_handle_request_start` (`threadworker.go:273-275`).

   Two consequences for whoever designs the arena (#11). First, reclaiming only
   in `go_frankenphp_after_script_execution` is a regular-mode rule; applied
   uniformly it passes every regular-mode test and then grows a worker
   monotonically: the only thing that ends a worker script on a healthy thread is
   the `max_requests` reboot, and `max_requests` defaults to 0, which upstream
   documents as unlimited for both handler kinds (`options.go:170`) and guards on
   with `maxRequestsPerThread > 0` (`threadworker.go:219-220`). So in the default
   configuration the reclaim point never fires — and worker mode is precisely the
   mode the benchmark exists to measure. Second,
   the worker rule means a request's arena deliberately outlives that request:
   it is still allocated after the response is sent and is only released when the
   *next* request begins. So the arena may not be tied to response completion
   either.
3. **libc-`malloc`'d by the callback, `free`'d by C.** `go_read_cookies`
   (called at `frankenphp.c:1195-1196`) returns a pointer C stores into
   `SG(request_info).cookie_data` and later releases with libc `free()`
   (`frankenphp.c:362-364`). `go_get_custom_php_ini` (called at
   `frankenphp.c:1681` / `:1685`) returns a pointer C assigns to
   `frankenphp_sapi_module.ini_entries` and, again, releases with `free()`
   (`frankenphp.c:1722-1724`). Rust's `CString::into_raw` uses Rust's global
   allocator, not libc's `malloc`; on any target where those two are not
   provably the same allocator, handing C a `CString::into_raw` pointer for
   it to `free()` is undefined behaviour. These returns must come from an
   explicit `libc::malloc` call instead.
4. **Persistent, interned, never freed.** `frankenphp_init_persistent_string`
   (`frankenphp.c:1278-1287`) creates a `zend_string` with
   `zend_string_init(..., /* persistent */ 1)` and flags it
   `IS_STR_INTERNED`, so it is ignored by the per-request GC and lives for the
   process's lifetime. Used for the fixed set of hard-coded server-variable
   keys (`frankenphp_init_interned_strings`, `frankenphp.c:1290+`). There is
   no Rust-side deallocation to write for these at all; writing one would be
   the bug.

## What is deliberately out of scope

`docs/PORTING-NOTES.md:172-177` is authoritative; summarized: TLS, HTTP/2,
HTTP/3, static file serving, Caddyfile parsing, the admin API, Mercure,
Vulcain, metrics, the file watcher, `internal/extgen`, and autoscaling are not
implemented and are not to be stubbed in a way that pretends otherwise — where
upstream calls into these, the Rust side returns a clearly-named unimplemented
result and logs once.

## Open design questions

These are named here so agents don't invent answers for them:

- The concrete Rust encoding of `threadHandler` (trait object vs. enum vs.
  something else) is undecided; that is issue #10's job, not this document's.
- The arena design for request-lifetime-borrowed strings (ownership scheme 2
  above) is undecided; whichever issue implements `context.rs` (#11) owns
  that decision.
