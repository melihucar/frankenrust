//! Port of `vendor/frankenphp/threadregular.go` (203 lines): the
//! non-worker `threadHandler` that executes one PHP script per request, and
//! the dispatch functions that hand a request to a regular PHP thread from
//! outside it.
//!
//! Worker mode (`threadworker.go`) is issue #14's; this file is the
//! `regularThread` side only.
//!
//! # The async/pthread boundary this file is one half of
//!
//! [`handle_request_with_regular_php_threads`] is the direct port of
//! upstream's `handleRequestWithRegularPHPThreads` (`threadregular.go:137-186`),
//! **minus** the final `<-ch.frankenPHPContext.done` wait. Upstream can block
//! a whole goroutine on that receive for free; a goroutine is cheap and the
//! Go scheduler multiplexes thousands of them onto a handful of OS threads.
//! Rust has no equivalent free lunch: this function runs synchronously
//! (blocking, in the crossbeam sense, on the shared-channel fallback path)
//! and must never run on a tokio worker thread, so `frankenrust-server`
//! calls it inside `spawn_blocking` and `.await`s the completion signal
//! **separately**, on the tokio side, once this call returns having hung the
//! request off a PHP thread's channel (`docs/PORTING-NOTES.md:130-147`'s
//! diagram: "send on a crossbeam channel" and "await oneshot::Receiver" are
//! two different steps for us, where upstream's single blocking receive did
//! both at once).
//!
//! # Directed dispatch is a true rendezvous, not a queue
//!
//! The fast path below is `crossbeam_channel::bounded(0)::try_send`, which
//! succeeds **only** if a `RegularThread` is parked inside
//! [`RegularThread::wait_for_request`]'s `select!` at that exact instant
//! (`threadregular.go:145-155`'s `select { case thread.requestChan <- ch:
//! default: }` on Go's own unbuffered channel has the identical rendezvous
//! semantics). A `Mutex<Option<RequestContext>>` plus a condvar would not
//! reproduce this -- it would hand work to a thread that has not yet reached
//! its park point, which is exactly the bug class this port must not
//! introduce (see this issue's body).

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use crossbeam_channel::{bounded, select, Receiver, Sender, TrySendError};

use crate::context::{RequestContext, CONTEXT_SLOTS};
use crate::state::State;
use crate::thread::{InactivePhpThreadClaim, PhpThread, ScriptPath, ThreadHandler};

/// `regularThread` (`threadregular.go:19-25`), minus `contextHolder` and
/// `requestCount`: the request itself now travels as the channel payload
/// (see [`before_script_execution`]'s SAFETY-adjacent note below on why no
/// separate `contextMu`-guarded field is needed here), and `requestCount`
/// backed `max_requests` (`threadregular.go:92-104`), which this issue does
/// not implement (out of scope: "max_requests and the reboot() branch of
/// waitForRequest").
///
/// `own_receiver` is the Rust encoding of `phpThread.requestChan`
/// (`phpthread.go:22`, `:55`): upstream puts the directed channel on the
/// *thread*, not the handler, so it survives a handler churn a thread never
/// actually undergoes while staying a regular thread. This port cannot add a
/// field to [`PhpThread`] (out of this issue's lane -- `thread.rs` is #10's),
/// so the channel instead lives on the handler and the matching [`Sender`]
/// is registered into [`REGULAR_THREADS`] for exactly as long as this
/// handler instance is attached, which is observably the same lifetime.
struct RegularThread {
    own_receiver: Receiver<RequestContext>,
}

impl RegularThread {
    /// One directed rendezvous channel per handler instance, matching the
    /// bound(0)/unbuffered `make(chan contextHolder)` at `phpthread.go:55`.
    fn new() -> (Self, Sender<RequestContext>) {
        let (sender, receiver) = bounded(0);
        (
            Self {
                own_receiver: receiver,
            },
            sender,
        )
    }

    /// `waitForRequest` (`threadregular.go:91-127`), minus the
    /// `max_requests` reboot check at `:92-104` (out of scope for this
    /// issue) and the metrics calls (out of scope project-wide, per
    /// `docs/ARCHITECTURE.md`'s "what is deliberately out of scope").
    ///
    /// The three-way `select` (`:110-116`) becomes
    /// `crossbeam_channel::select!` over: this generation's drain receiver,
    /// fetched fresh on every call (a receiver cached across a close/reopen
    /// is a lost wakeup -- see `thread.rs`'s `DrainReceiver` doc comment and
    /// this issue's body); the shared fan-out channel; and this handler's
    /// own directed channel.
    fn wait_for_request(&self, thread: &PhpThread) -> WaitOutcome {
        thread.state_machine().mark_as_waiting(true);
        let drain = thread.drain_receiver();
        let shared = shared_channel();

        let outcome = select! {
            recv(drain.receiver()) -> _ => WaitOutcome::Drained,
            recv(&shared.receiver) -> msg => WaitOutcome::Delivered(Box::new(msg.ok())),
            recv(&self.own_receiver) -> msg => WaitOutcome::Delivered(Box::new(msg.ok())),
        };

        // Upstream only clears MarkAsWaiting(false) on the "received work"
        // arm (`threadregular.go:106-123`), leaving it `true` across the
        // recursive `beforeScriptExecution()` call the drain arm makes
        // (`:111-113`). That asymmetry only affects the diagnostic
        // "waiting since" timestamp `debugstate.go` reads, not correctness,
        // so this port clears it unconditionally instead of reproducing the
        // asymmetry.
        thread.state_machine().mark_as_waiting(false);
        outcome
    }
}

enum WaitOutcome {
    /// This generation's drain channel closed: a state transition or
    /// shutdown is waiting. Loop back to [`before_script_execution`]'s top
    /// and let the state read there decide what to do next.
    Drained,
    /// A sender delivered a request, or a channel disconnected without ever
    /// delivering one (`None` -- treated as a spurious wakeup: loop back and
    /// re-read state, exactly like `Drained`). Boxed: `RequestContext` is
    /// hundreds of bytes and `Drained` carries none, so an unboxed payload
    /// here would size every `WaitOutcome` -- including the common `Drained`
    /// case -- to the largest variant.
    Delivered(Box<Option<RequestContext>>),
}

impl ThreadHandler for RegularThread {
    fn name(&self) -> &str {
        "Regular PHP Thread"
    }

    /// `beforeScriptExecution` (`threadregular.go:42-70`), rewritten as a
    /// loop instead of the upstream tail recursion: a PHP pthread has a
    /// fixed native stack (see `thread_inactive.rs`'s `InactiveThread`,
    /// which already makes this same translation for `threadinactive.go`).
    fn before_script_execution(&self, thread: &PhpThread) -> Option<ScriptPath> {
        loop {
            match thread.state_machine().get() {
                State::TransitionRequested => {
                    detach_regular_thread(thread);
                    return thread.transition_to_new_handler();
                }

                State::TransitionComplete => {
                    // SAFETY: this call writes TSRM-local state --
                    // `is_worker_thread` is `THREAD_LOCAL`
                    // (`frankenphp.c:122`), and `PG(ignore_user_abort)` is
                    // TSRM-local storage -- so it is only sound on the PHP
                    // pthread that owns it. `before_script_execution` is
                    // reached only from `go_frankenphp_before_script_execution`
                    // (`callbacks/thread.rs`), which C calls synchronously
                    // from `php_thread()` on the pthread that owns
                    // `thread_index` (`frankenphp.c:1506`, `docs/PORTING-NOTES.md`'s
                    // "PHP never runs on a Rust async task" rule) -- exactly
                    // the thread this call must run on. `false`: regular
                    // threads are never workers (`threadregular.go:50`).
                    unsafe {
                        frankenrust_sys::frankenphp_update_local_thread_context(false);
                    }
                    thread.state_machine().set(State::Ready);
                }

                State::Ready => match self.wait_for_request(thread) {
                    WaitOutcome::Drained => {}
                    WaitOutcome::Delivered(boxed) => match *boxed {
                        None => {}
                        Some(ctx) => {
                            // `return handler.contextHolder.frankenPHPContext.scriptFilename`
                            // (`:126`), read before the context moves into
                            // `CONTEXT_SLOTS` below -- `ScriptPath::new` needs
                            // an owned copy and `set` takes `ctx` by value.
                            let script_filename = ctx.script_filename.clone();
                            CONTEXT_SLOTS.set(thread.index(), ctx);
                            return ScriptPath::new(script_filename);
                        }
                    },
                },

                State::Rebooting | State::ForceRebooting => return None,

                // `handler.requestCount = 0` (`:59`) has no counterpart: see
                // this struct's doc comment on why `requestCount` does not
                // exist here. Falls through to `Ready` on the next
                // iteration, same as upstream's own tail call into
                // `waitForRequest`.
                State::RebootReady => thread.state_machine().set(State::Ready),

                State::ShuttingDown => {
                    detach_regular_thread(thread);
                    return None;
                }

                unexpected => {
                    // Upstream panics here (`:69`). See `thread_inactive.rs`'s
                    // identical fallback for why this port instead preserves
                    // the slot and waits for a state it recognizes: panicking
                    // through this `extern "C"` callback would abort the
                    // process, and returning NULL would strand the slot in
                    // Done with no controller left to restore Reserved.
                    write_diagnostic(format_args!(
                        "regular PHP thread {} observed unexpected state {unexpected}",
                        thread.index()
                    ));
                    thread.state_machine().wait_for(&[
                        State::TransitionRequested,
                        State::TransitionComplete,
                        State::Ready,
                        State::Rebooting,
                        State::ForceRebooting,
                        State::RebootReady,
                        State::ShuttingDown,
                    ]);
                }
            }
        }
    }

    /// `afterScriptExecution` + `afterRequest` (`threadregular.go:72-75`,
    /// `:129-135`), minus the `requestCount` increment (see this module's
    /// "out of scope" notes). `close_context` then `clear` as two sequential
    /// calls, never nested -- `context.rs`'s `ContextSlots` rule 2 forbids
    /// re-entering the table from inside a `with_context_mut` closure, and a
    /// completion signal fired by `close_context` is exactly the shape that
    /// rule calls out.
    fn after_script_execution(&self, thread: &PhpThread, _exit_status: i32) {
        CONTEXT_SLOTS.with_context_mut(thread.index(), |ctx| {
            if let Some(ctx) = ctx {
                ctx.close_context();
            }
        });
        CONTEXT_SLOTS.clear(thread.index());
    }
}

/// `convertToRegularThread` (`threadregular.go:34-40`): installs a fresh
/// [`RegularThread`] handler on an inactive slot and, only if that
/// succeeded, attaches it to the dispatch registry. Upstream calls
/// `attachRegularThread` unconditionally; this is stricter because
/// [`InactivePhpThreadClaim::assign_handler`] reports failure explicitly,
/// where upstream's `setHandler` does not, and attaching a thread the
/// handler swap did not actually land on would register a directed channel
/// nobody drains.
///
/// `assign_handler` releases `thread.rs`'s `SCALING` lock before returning
/// (that lock is not this issue's to hold across the following
/// [`attach_regular_thread`] call -- `thread.rs` is #10's), so a
/// `drain_php_threads()` interleaved between the two calls can retire the
/// pool and then observe a fresh registry entry pushed in behind it. Filed
/// as #172: inert (a stale entry's `Sender` has no parked receiver, so a
/// directed `try_send` against it just fails over like a busy thread) and
/// not fixable in this issue's lane, since closing it needs either a
/// `thread.rs` change or resetting [`REGULAR_THREADS`] from
/// `init_php_threads`, also `thread.rs`'s.
pub fn convert_to_regular_thread(claim: InactivePhpThreadClaim) -> Option<Arc<PhpThread>> {
    let (handler, sender) = RegularThread::new();
    let thread = claim.assign_handler(Arc::new(handler))?;
    attach_regular_thread(Arc::clone(&thread), sender);
    Some(thread)
}

/// One entry in [`REGULAR_THREADS`]: `regularThreads []*phpThread`
/// (`threadregular.go:28`) plus the directed [`Sender`] upstream reaches
/// through `thread.requestChan` (`:146`) -- see [`RegularThread`]'s doc
/// comment for why the sender lives in the registry rather than on
/// [`PhpThread`] itself.
struct RegularThreadEntry {
    thread: Arc<PhpThread>,
    sender: Sender<RequestContext>,
}

static REGULAR_THREADS: RwLock<Vec<RegularThreadEntry>> = RwLock::new(Vec::new());

fn attach_regular_thread(thread: Arc<PhpThread>, sender: Sender<RequestContext>) {
    let mut threads = REGULAR_THREADS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    threads.push(RegularThreadEntry { thread, sender });
}

/// `detachRegularThread` (`:194-203`). Called only from the two
/// [`ThreadHandler::before_script_execution`] branches that end this
/// handler's tenure -- `TransitionRequested` and `ShuttingDown` -- never
/// from `Rebooting`/`ForceRebooting`: see this issue's body on why a
/// rebooting thread deliberately stays a dispatch target (the directed send
/// staying non-blocking is what makes that safe).
fn detach_regular_thread(thread: &PhpThread) {
    let mut threads = REGULAR_THREADS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    threads.retain(|entry| entry.thread.index() != thread.index());
}

struct SharedChannel {
    sender: Sender<RequestContext>,
    receiver: Receiver<RequestContext>,
}

/// `regularRequestChan` (`:30`, `make(chan contextHolder)` at
/// `frankenphp.go:324`): one unbuffered/rendezvous channel every regular
/// thread's `wait_for_request` selects on, and every queued request is
/// eventually sent to.
static SHARED_REQUEST_CHANNEL: OnceLock<SharedChannel> = OnceLock::new();

fn shared_channel() -> &'static SharedChannel {
    SHARED_REQUEST_CHANNEL.get_or_init(|| {
        let (sender, receiver) = bounded(0);
        SharedChannel { sender, receiver }
    })
}

/// Upstream's `queuedRegularThreads` (`:31`).
static QUEUED_REGULAR_THREADS: AtomicI64 = AtomicI64::new(0);

/// `handleRequestWithRegularPHPThreads` (`:137-186`), minus:
///
/// - `metrics.*` calls -- metrics are out of scope project-wide
///   (`docs/ARCHITECTURE.md`).
/// - the `scaleChan` and `timeoutChan(maxWaitTime)` `select` arms
///   (`:173-183`) -- autoscaling and `ErrMaxWaitTimeExceeded`, both out of
///   scope for this issue (see this issue's body: "do not invent a timeout
///   to fill the hole"). With both arms gone, the retry loop collapses to
///   one plain blocking send on the shared channel.
/// - the final `<-ch.frankenPHPContext.done` wait (`:148`, `:169`) -- see
///   this module's top-of-file doc comment for why that half of the
///   diagram lives in `frankenrust-server`, on the tokio side, instead.
///
/// Ownership of `work` moves to whichever thread actually receives it (via
/// its directed channel or the shared one); if neither ever does (every
/// receiver dropped -- practically, no regular thread exists), `work` is
/// dropped here and its `CompletionSignal` never fires, which the caller's
/// `oneshot::Receiver` observes as a disconnect rather than a hang. That is
/// the *only* outcome if a caller reaches this function after the pool has
/// drained, though: unlike upstream's `ErrNotRunning`
/// (`frankenphp.go:402-404`), an empty [`REGULAR_THREADS`] with a live
/// [`SHARED_REQUEST_CHANNEL`] receiver (a `'static` that outlives any one
/// pool) falls through to a blocking send that then parks forever.
/// `frankenrust-server`'s own `run` never *starts* a dispatch after its drain,
/// but that is weaker than it sounds: a client that disconnects mid-request
/// makes hyper drop the `handle` future while the `spawn_blocking` task it
/// spawned still owns the [`RequestContext`], and that orphan can reach the
/// send below after `drain_php_threads` has already retired the pool. Filed as
/// #174 (point 2), together with (point 1) the missing `runtime.Gosched()`
/// yield this port does apply, just below.
pub fn handle_request_with_regular_php_threads(mut work: RequestContext) {
    // `runtime.Gosched()` (`:140`), reproduced as `yield_now`: safe here
    // because this function's own doc comment establishes it never runs on a
    // tokio worker thread (`frankenrust-server` always reaches it through
    // `spawn_blocking`, and #173 tracks that call itself costing a
    // blocking-pool thread per *queued*, not per executing, request). Gives a
    // PHP thread that just returned from `after_script_execution` a chance to
    // reach `wait_for_request`'s `select!` and park before the directed scan
    // below runs, which is what makes the fast path hit at all under load.
    std::thread::yield_now();

    if QUEUED_REGULAR_THREADS.load(Ordering::SeqCst) == 0 {
        let threads = REGULAR_THREADS
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        for entry in threads.iter() {
            match entry.sender.try_send(work) {
                Ok(()) => return,
                Err(TrySendError::Full(returned) | TrySendError::Disconnected(returned)) => {
                    work = returned;
                }
            }
        }
    }

    QUEUED_REGULAR_THREADS.fetch_add(1, Ordering::SeqCst);
    let delivered = shared_channel().sender.send(work);
    QUEUED_REGULAR_THREADS.fetch_sub(1, Ordering::SeqCst);

    if delivered.is_err() {
        write_diagnostic(format_args!(
            "a queued request found no regular PHP thread to receive it \
             (shared dispatch channel has no live receiver)"
        ));
    }
}

fn write_diagnostic(arguments: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "frankenrust: {arguments}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // `PhpThread` has no constructor reachable outside `thread.rs` (its
    // `new`/`with_launcher` are private to that module, and the stand-in
    // launcher used by *its own* tests lives inside `thread.rs`'s private
    // `mod tests` -- neither is `pub`/`pub(crate)`, and adding either would
    // mean editing a file this issue does not own). So this module's own
    // unit tests exercise only what is testable without a `PhpThread`
    // instance: the channel rendezvous semantics
    // ([`RegularThreadEntry::sender`]'s directed try-send) and the
    // shared-channel fallback path of [`handle_request_with_regular_php_threads`]
    // with an empty [`REGULAR_THREADS`]. `convert_to_regular_thread`,
    // `attach_regular_thread`/`detach_regular_thread`, and the fast
    // directed-dispatch path *with* a real registered thread are exercised
    // end-to-end by `frankenrust-server`'s integration and concurrency
    // tests instead, against real booted PHP threads.

    fn context_with_script(script_filename: &str) -> RequestContext {
        use crate::context::CompletionSignal;
        let mut ctx = RequestContext::new(String::new(), None, None, CompletionSignal::none());
        ctx.script_filename = script_filename.as_bytes().to_vec();
        ctx
    }

    #[test]
    fn directed_try_send_only_succeeds_against_a_parked_receiver() {
        let (sender, receiver) = bounded::<RequestContext>(0);

        // Nobody is parked on `receiver` yet: a non-blocking send must fail,
        // not silently queue the work where nothing is waiting for it.
        let result = sender.try_send(context_with_script("/a.php"));
        assert!(
            result.is_err(),
            "an unbuffered channel must reject a send with no parked receiver"
        );

        let parked = std::thread::spawn(move || receiver.recv().map(|ctx| ctx.script_filename));
        // Give the spawned thread a chance to reach `recv` and park. This is
        // a best-effort wait, not a correctness requirement: if it loses the
        // race the loop below simply retries.
        let mut delivered = false;
        for _ in 0..200 {
            if sender.try_send(context_with_script("/b.php")).is_ok() {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            delivered,
            "try_send never rendezvoused with the parked receiver"
        );
        assert_eq!(
            parked.join().unwrap().unwrap(),
            b"/b.php",
            "the parked receiver must observe the delivered request"
        );
    }

    #[test]
    fn shared_channel_fallback_delivers_and_tracks_the_queued_counter() {
        // With no regular thread registered, dispatch must fall all the way
        // through to the shared channel rather than silently dropping the
        // request. This test owns no shared global except
        // `SHARED_REQUEST_CHANNEL`/`QUEUED_REGULAR_THREADS` (process-wide by
        // necessity -- they back the real dispatch path) and never touches
        // `REGULAR_THREADS`, so it cannot race the fast-path tests other
        // files might add later.
        let receiver = shared_channel().receiver.clone();
        let before = QUEUED_REGULAR_THREADS.load(Ordering::SeqCst);

        let worker = std::thread::spawn(move || receiver.recv().map(|ctx| ctx.script_filename));
        handle_request_with_regular_php_threads(context_with_script("/shared.php"));

        assert_eq!(
            worker.join().unwrap().unwrap(),
            b"/shared.php",
            "the shared channel must deliver the dispatched request"
        );
        assert_eq!(
            QUEUED_REGULAR_THREADS.load(Ordering::SeqCst),
            before,
            "the queued counter must return to its prior value once the send completes"
        );
    }
}
