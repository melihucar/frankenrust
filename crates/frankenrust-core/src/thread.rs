//! PHP main-thread and per-thread lifecycle registry.
//!
//! C creates and owns the PHP pthreads (`frankenphp.c:1471-1619`,
//! `:1637-1760`). Rust owns only their registry slots, lifecycle state, handler
//! rendezvous, and the channels which wake parked handlers.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::os::raw::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use frankenrust_sys::{
    force_kill_slot, frankenphp_destroy_thread_metrics, frankenphp_force_kill_thread,
    frankenphp_get_current_memory_limit, frankenphp_init_thread_metrics,
    frankenphp_new_main_thread, frankenphp_new_php_thread, frankenphp_release_thread_for_kill,
};

use crate::state::{State, ThreadState};
use crate::thread_inactive::InactiveThread;

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// The caller's requested `max_threads`: an explicit ceiling, or automatic
/// sizing from the PHP memory limit and total system memory
/// (`phpmainthread.go:259-278`, `setAutomaticMaxThreads`).
///
/// Upstream encodes `auto` as a negative `int` and returns early once
/// `maxThreads >= 0` (`phpmainthread.go:263-265`); this enum makes that an
/// exhaustive match instead of a sentinel value threaded through arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaxThreads {
    Fixed(usize),
    Auto,
}

/// Errors which can occur while starting the PHP thread registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadError {
    AlreadyInitialized,
    InvalidThreadCount {
        num_threads: usize,
        max_threads: MaxThreads,
    },
    MainThreadCreation {
        code: c_int,
    },
    PhpThreadCreation {
        thread_index: usize,
    },
    InvalidBootState {
        thread_index: usize,
        state: State,
    },
}

impl fmt::Display for ThreadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => formatter.write_str("PHP threads are already initialized"),
            Self::InvalidThreadCount {
                num_threads,
                max_threads,
            } => write!(
                formatter,
                "invalid PHP thread counts: num_threads={num_threads}, max_threads={max_threads:?}"
            ),
            Self::MainThreadCreation { code } => {
                write!(formatter, "unable to create PHP main thread (code {code})")
            }
            Self::PhpThreadCreation { thread_index } => {
                write!(formatter, "unable to create PHP thread {thread_index}")
            }
            Self::InvalidBootState {
                thread_index,
                state,
            } => write!(
                formatter,
                "cannot boot PHP thread {thread_index} from state {state}"
            ),
        }
    }
}

impl Error for ThreadError {}

/// A non-empty script path which C may borrow through the next
/// `go_frankenphp_after_script_execution` callback.
///
/// The constructor narrows upstream's empty-string stop sentinel before a
/// handler can return a path. Once a handler returns `Some(ScriptPath)`, the
/// callback always publishes it: it must not discard a request which the
/// handler may already have dequeued. Interior NUL bytes are preserved, like
/// Go's `pinCString`; C observes the prefix before the first NUL.
pub struct ScriptPath {
    bytes: Box<[u8]>,
}

impl ScriptPath {
    pub fn new(mut bytes: Vec<u8>) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        bytes.push(0);
        Some(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }

    fn as_mut_ptr(&mut self) -> *mut i8 {
        self.bytes.as_mut_ptr().cast::<i8>()
    }
}

/// One receiver from a particular drain-channel generation.
///
/// Regular and worker handlers compose [`Self::receiver`] with their request
/// channels using `crossbeam_channel::select!`. A disconnect means this exact
/// generation was drained; a later generation has a different receiver and a
/// strictly newer wrapping tag.
#[derive(Clone)]
pub struct DrainReceiver {
    generation: u64,
    receiver: Receiver<()>,
}

impl DrainReceiver {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn receiver(&self) -> &Receiver<()> {
        &self.receiver
    }
}

/// The live half of one drain-channel generation.
///
/// `receiver` stays reachable through [`DrainChannel::receiver`] for the
/// entire close/reopen window; only `sender` is one-shot per generation.
/// Upstream's `drainChan` field always holds a channel value, closed or not
/// (`phpthread.go:23`, `:124`, `:162`) — a `select` composed against it after
/// a close fires immediately. An earlier version of this file stored the
/// receiver inside the same `Option` that gated the sender, so
/// `DrainChannel::receiver` returned `None` for the whole window between
/// `close()` and the matching `reopen()`. A handler that fetched a receiver
/// in that window had nothing to select on: parked on its request channel
/// alone, it never observed `ShuttingDown`, and the unbounded post-force-kill
/// `Done` wait in `PhpThread::shutdown_locked` hung forever. Keeping the
/// receiver unconditional restores upstream's "always a channel" invariant.
struct DrainGeneration {
    generation: u64,
    sender: Option<Sender<()>>,
    receiver: Receiver<()>,
}

struct ClosedDrainGeneration {
    generation: u64,
}

/// Close/reopen storage for upstream's `drainChan`.
///
/// Closing takes only the [`Sender`] out of the current generation, so
/// [`Self::receiver`] keeps returning a valid (possibly already-disconnected)
/// receiver across the whole close/reopen window. Taking the sender is a
/// one-shot `Option::take`, so the same generation cannot be closed twice by
/// construction; the returned [`ClosedDrainGeneration`] is consumed by value
/// on reopen, so it cannot be replayed either.
struct DrainChannel {
    current: Mutex<DrainGeneration>,
}

impl DrainChannel {
    fn new() -> Self {
        Self {
            current: Mutex::new(Self::generation(0)),
        }
    }

    fn generation(generation: u64) -> DrainGeneration {
        let (sender, receiver) = unbounded();
        DrainGeneration {
            generation,
            sender: Some(sender),
            receiver,
        }
    }

    fn receiver(&self) -> DrainReceiver {
        let current = lock_mutex(&self.current);
        DrainReceiver {
            generation: current.generation,
            receiver: current.receiver.clone(),
        }
    }

    fn close(&self) -> Option<ClosedDrainGeneration> {
        let mut current = lock_mutex(&self.current);
        let sender = current.sender.take()?;
        drop(sender);
        Some(ClosedDrainGeneration {
            generation: current.generation,
        })
    }

    fn reopen(&self, closed: ClosedDrainGeneration) {
        let mut current = lock_mutex(&self.current);
        if current.generation == closed.generation && current.sender.is_none() {
            *current = Self::generation(closed.generation.wrapping_add(1));
        }
    }

    fn renew(&self) {
        if let Some(closed) = self.close() {
            self.reopen(closed);
        }
    }
}

/// Behavior attached to a C-created PHP thread.
///
/// These methods run in callbacks on the PHP pthread which owns `thread`.
/// `before_script_execution` may block for work. It must return `None` only
/// before dequeuing work; C calls `after_script_execution` only after a
/// non-NULL path, so returning `None` after a dequeue strands that request.
pub trait ThreadHandler: Send + Sync {
    fn name(&self) -> &str;
    fn before_script_execution(&self, thread: &PhpThread) -> Option<ScriptPath>;
    fn after_script_execution(&self, thread: &PhpThread, exit_status: i32);
}

/// Handler storage with separate transition serialization and value locks.
///
/// `writer` may be held across the state rendezvous because the PHP pthread
/// does not need it to publish `TransitionInProgress`. The `RwLock` protects
/// each `Arc` clone and replacement even if safe callers misuse the public
/// state API and violate the expected lifecycle ordering.
struct HandlerSlot {
    writer: Mutex<()>,
    value: RwLock<Option<Arc<dyn ThreadHandler>>>,
}

impl HandlerSlot {
    fn new() -> Self {
        Self {
            writer: Mutex::new(()),
            value: RwLock::new(None),
        }
    }

    fn writer(&self) -> MutexGuard<'_, ()> {
        lock_mutex(&self.writer)
    }

    fn replace(&self, _writer: &MutexGuard<'_, ()>, handler: Arc<dyn ThreadHandler>) {
        *write_rwlock(&self.value) = Some(handler);
    }

    fn read_while_stable(&self) -> Option<Arc<dyn ThreadHandler>> {
        // Do not take `writer` here: upstream's setHandler holds its writer
        // while waiting for this callback to publish TransitionInProgress.
        // The independent value lock protects the clone without blocking that
        // state handshake (`phpthread.go:151-178`, ARCHITECTURE.md:296-310).
        read_rwlock(&self.value).clone()
    }

    fn read_with_writer(&self, _writer: &MutexGuard<'_, ()>) -> Option<Arc<dyn ThreadHandler>> {
        read_rwlock(&self.value).clone()
    }
}

/// Raw `EG()` pointers and the pthread identifier captured by C.
struct StoredForceKillSlot(force_kill_slot);

// SAFETY: Rust never dereferences these pointers. C receives the slot by value
// only under `PhpThread::force_kill`'s read lock; store and clear take the write
// lock, and clear completes before C calls `ts_free_thread`, so the pointed-to
// TSRM storage cannot be freed concurrently with a kill.
unsafe impl Send for StoredForceKillSlot {}

// SAFETY: shared access is read-only under the same read/write-lock invariant;
// only the owning PHP pthread stores or clears a slot.
unsafe impl Sync for StoredForceKillSlot {}

trait PhpThreadLauncher: Send + Sync {
    fn launch(&self, thread: Arc<PhpThread>) -> bool;
}

struct CPhpThreadLauncher;

impl PhpThreadLauncher for CPhpThreadLauncher {
    fn launch(&self, thread: Arc<PhpThread>) -> bool {
        // SAFETY: production constructs `PhpThread` only as an installed
        // registry slot. `boot` installs its handler before reaching here, and
        // C establishes `ts_resource(0)` on the new pthread before any Rust
        // lifecycle callback runs (`frankenphp.c:1489-1497`).
        unsafe { frankenphp_new_php_thread(thread.thread_index) }
    }
}

/// Rust-side state associated with one C-created PHP pthread.
pub struct PhpThread {
    thread_index: usize,
    state: ThreadState,
    handler: HandlerSlot,
    drain: DrainChannel,
    script_path: Mutex<Option<ScriptPath>>,
    force_kill: RwLock<Option<StoredForceKillSlot>>,
    launcher: Arc<dyn PhpThreadLauncher>,
}

impl PhpThread {
    fn new(thread_index: usize) -> Self {
        Self::with_launcher(thread_index, Arc::new(CPhpThreadLauncher))
    }

    fn with_launcher(thread_index: usize, launcher: Arc<dyn PhpThreadLauncher>) -> Self {
        Self {
            thread_index,
            state: ThreadState::new(),
            handler: HandlerSlot::new(),
            drain: DrainChannel::new(),
            script_path: Mutex::new(None),
            force_kill: RwLock::new(None),
            launcher,
        }
    }

    pub const fn index(&self) -> usize {
        self.thread_index
    }

    /// Returns a read-only snapshot of this slot's lifecycle state.
    ///
    /// The underlying [`ThreadState`] is deliberately not exposed through the
    /// safe public handle: publishing lifecycle transitions can release the
    /// main pthread into `tsrm_shutdown()`, so only the in-crate lifecycle
    /// controllers may mutate it.
    ///
    /// ```compile_fail
    /// use frankenrust_core::state::State;
    /// use frankenrust_core::thread::php_threads;
    ///
    /// let thread = php_threads().into_iter().next().unwrap();
    /// thread.state().set(State::Done);
    /// ```
    pub fn state(&self) -> State {
        self.state.get()
    }

    pub(crate) const fn state_machine(&self) -> &ThreadState {
        &self.state
    }

    /// A receiver for the currently-live drain generation.
    ///
    /// Always returns a valid receiver, closed or not: [`DrainChannel`] keeps
    /// one live across the whole close/reopen window, matching upstream's
    /// `drainChan` field. Compose this with a request channel in
    /// `crossbeam_channel::select!`; a disconnect on it means this exact
    /// generation was drained.
    pub fn drain_receiver(&self) -> DrainReceiver {
        self.drain.receiver()
    }

    pub fn name(&self) -> String {
        let writer = self.handler.writer();
        self.handler.read_with_writer(&writer).map_or_else(
            || "unknown".to_string(),
            |handler| handler.name().to_string(),
        )
    }

    fn boot(self: &Arc<Self>) -> Result<(), ThreadError> {
        if !self.state.compare_and_swap(State::Reserved, State::Booting)
            && !self
                .state
                .compare_and_swap(State::BootRequested, State::Booting)
        {
            return Err(ThreadError::InvalidBootState {
                thread_index: self.thread_index,
                state: self.state.get(),
            });
        }

        let writer = self.handler.writer();
        self.handler.replace(&writer, Arc::new(InactiveThread));
        self.drain.renew();
        drop(writer);

        if !self.launcher.launch(Arc::clone(self)) {
            self.state.set(State::Reserved);
            return Err(ThreadError::PhpThreadCreation {
                thread_index: self.thread_index,
            });
        }

        self.state.wait_for(&[State::Inactive]);
        Ok(())
    }

    /// Safely changes this slot's handler from outside the PHP pthread.
    ///
    /// Taking the process-wide scaling lock here makes the exclusion with
    /// `drain_php_threads` part of the public API rather than a caller
    /// convention.
    pub fn set_handler(&self, handler: Arc<dyn ThreadHandler>) -> bool {
        let _scaling = lock_mutex(&SCALING);
        self.set_handler_locked(handler)
    }

    fn set_handler_locked(&self, handler: Arc<dyn ThreadHandler>) -> bool {
        let writer = self.handler.writer();
        if !self
            .state
            .request_safe_state_change(State::TransitionRequested)
        {
            return false;
        }

        // `close(thread.drainChan)` immediately follows the state change in
        // upstream. A regular/worker handler selects this generation alongside
        // request channels, so it cannot remain parked while the controller
        // waits for TransitionInProgress.
        let Some(closed_drain) = self.drain.close() else {
            write_diagnostic(format_args!(
                "thread {} had no open drain generation during handler transition",
                self.thread_index
            ));
            return false;
        };

        self.state.wait_for(&[State::TransitionInProgress]);
        self.handler.replace(&writer, handler);
        self.drain.reopen(closed_drain);
        self.state.set(State::TransitionComplete);
        true
    }

    pub(crate) fn transition_to_new_handler(&self) -> Option<ScriptPath> {
        self.state.set(State::TransitionInProgress);
        self.state.wait_for(&[State::TransitionComplete]);
        self.handler
            .read_while_stable()
            .and_then(|handler| handler.before_script_execution(self))
    }

    pub(crate) fn before_script_execution(&self) -> Option<ScriptPath> {
        self.handler
            .read_while_stable()
            .and_then(|handler| handler.before_script_execution(self))
    }

    pub(crate) fn after_script_execution(&self, exit_status: i32) {
        if let Some(handler) = self.handler.read_while_stable() {
            handler.after_script_execution(self, exit_status);
        }
    }

    pub(crate) fn publish_script_path(&self, script_path: ScriptPath) -> *mut i8 {
        let mut pinned = lock_mutex(&self.script_path);
        *pinned = Some(script_path);
        pinned
            .as_mut()
            .map_or(std::ptr::null_mut(), ScriptPath::as_mut_ptr)
    }

    pub(crate) fn release_script_path(&self) {
        lock_mutex(&self.script_path).take();
    }

    pub(crate) fn store_force_kill_slot(&self, slot: force_kill_slot) {
        let mut current = write_rwlock(&self.force_kill);
        if let Some(previous) = current.take() {
            // SAFETY: the write lock excludes every killer. `previous` came
            // from C's registration callback for this index and is returned by
            // value to the matching C release function before it is replaced.
            unsafe { frankenphp_release_thread_for_kill(previous.0) };
        }
        *current = Some(StoredForceKillSlot(slot));
    }

    pub(crate) fn clear_force_kill_slot(&self) {
        let mut current = write_rwlock(&self.force_kill);
        if let Some(previous) = current.take() {
            // SAFETY: C calls this at frankenphp.c:1598 before
            // `ts_free_thread`. The write lock excludes concurrent killers, so
            // release completes before the TSRM-backed pointers become stale.
            unsafe { frankenphp_release_thread_for_kill(previous.0) };
        }
    }

    fn send_kill_signal(&self) {
        let current = read_rwlock(&self.force_kill);
        let Some(slot) = current.as_ref() else {
            return;
        };

        // SAFETY: this read guard stays held across C's dereferences and
        // excludes clear's write lock, which C invokes before freeing the EG()
        // storage. The slot is passed back verbatim to its C producer.
        unsafe { frankenphp_force_kill_thread(slot.0) };
    }

    fn begin_shutdown(&self) -> Option<ShutdownTicket> {
        if !self.state.request_safe_state_change(State::ShuttingDown) {
            let _ = self.state.compare_and_swap(State::Done, State::Reserved);
            return None;
        }

        // Upstream closes the drain generation immediately after publishing
        // ShuttingDown (`phpthread.go:118-124`). Force-kill cannot interrupt a
        // Rust channel receive, so this wake is required before any wait.
        let Some(closed_drain) = self.drain.close() else {
            write_diagnostic(format_args!(
                "thread {} had no open drain generation during shutdown",
                self.thread_index
            ));
            return None;
        };
        Some(ShutdownTicket { closed_drain })
    }

    fn finish_shutdown(&self, ticket: ShutdownTicket) {
        self.drain.reopen(ticket.closed_drain);
        self.state.set(State::Reserved);
    }

    /// Shuts this slot down and returns it to Reserved. The scaling lock is
    /// internal so a public caller cannot race this operation with a full
    /// registry drain.
    pub fn shutdown(&self) {
        let _scaling = lock_mutex(&SCALING);
        self.shutdown_locked();
    }

    fn shutdown_locked(&self) {
        let Some(ticket) = self.begin_shutdown() else {
            return;
        };

        if !self
            .state
            .wait_for_state_with_timeout(SHUTDOWN_GRACE_PERIOD, &[State::Done])
        {
            report_force_kill(self);
            self.send_kill_signal();
            // Intentionally unbounded. `php_main()` tears TSRM down as soon
            // as the main callback sees Done; giving up here would free global
            // engine state under a still-live PHP pthread (php/frankenphp#2573,
            // documented at phpmainthread.go:179-196).
            self.state.wait_for(&[State::Done]);
        }

        self.finish_shutdown(ticket);
    }

    pub(crate) fn on_thread_shutdown(&self) {
        // Upstream calls Unpin here as well as after each script. Ordinarily
        // the paired after-script callback already released this path; doing
        // it again is harmless and guarantees no request-lifetime path remains
        // when Done wakes a controller.
        self.release_script_path();
        match self.state.get() {
            State::Rebooting => self.state.set(State::RebootReady),
            State::ForceRebooting => self.state.set(State::YieldingForReboot),
            _ => self.state.set(State::Done),
        }
    }
}

struct ShutdownTicket {
    closed_drain: ClosedDrainGeneration,
}

/// State of the pthread which owns PHP module startup and TSRM teardown.
pub struct PhpMainThread {
    state: ThreadState,
    num_threads: usize,
    requested_max_threads: MaxThreads,
    max_threads: AtomicUsize,
    php_ini: Mutex<HashMap<String, String>>,
}

impl PhpMainThread {
    fn new(num_threads: usize, max_threads: MaxThreads, php_ini: HashMap<String, String>) -> Self {
        let initial = match max_threads {
            MaxThreads::Fixed(count) => count,
            // Placeholder only: nothing may observe `max_threads()` before
            // `finalize_max_threads` replaces this with the real resolution,
            // which runs before the main callback publishes Ready (#10's
            // initialisation-ordering rule).
            MaxThreads::Auto => num_threads,
        };
        Self {
            state: ThreadState::new(),
            num_threads,
            requested_max_threads: max_threads,
            max_threads: AtomicUsize::new(initial),
            php_ini: Mutex::new(php_ini),
        }
    }

    /// Returns a read-only snapshot of the main pthread's lifecycle state.
    ///
    /// In particular, safe callers cannot publish [`State::Done`]: that
    /// transition releases `php_main()` into TSRM teardown and belongs only to
    /// [`drain_php_threads`].
    ///
    /// ```compile_fail
    /// use frankenrust_core::state::State;
    /// use frankenrust_core::thread::main_thread;
    ///
    /// let main = main_thread().unwrap();
    /// main.state().set(State::Done);
    /// ```
    pub fn state(&self) -> State {
        self.state.get()
    }

    pub(crate) const fn state_machine(&self) -> &ThreadState {
        &self.state
    }

    pub const fn num_threads(&self) -> usize {
        self.num_threads
    }

    pub fn max_threads(&self) -> usize {
        self.max_threads.load(Ordering::Acquire)
    }

    /// `phpmainthread.go:250-253`: resolves `max_threads=auto`
    /// (`setAutomaticMaxThreads`, `:262-278`) if requested, then raises the
    /// result back up to `num_threads` if it came out lower. Runs only from
    /// `go_frankenphp_main_thread_is_ready`, on the main PHP pthread, before
    /// that callback publishes `Ready` (#10's initialisation-ordering rule).
    pub(crate) fn finalize_max_threads(&self) {
        let resolved = match self.requested_max_threads {
            MaxThreads::Fixed(_) => {
                resolve_max_threads(self.num_threads, self.requested_max_threads, 0, 0)
            }
            MaxThreads::Auto => {
                // SAFETY: `finalize_max_threads` runs only from
                // `go_frankenphp_main_thread_is_ready` (`frankenphp.c:1710`,
                // `callbacks/mainthread.rs`), on the main PHP pthread after
                // `frankenphp_new_main_thread` has completed `ts_resource(0)`
                // and PHP module startup, so `PG(memory_limit)` already holds
                // php.ini's parsed value. The C function takes no pointer
                // arguments and only reads that global (`frankenphp.c:1916`).
                let per_thread_limit = i64::from(unsafe { frankenphp_get_current_memory_limit() });
                resolve_max_threads(
                    self.num_threads,
                    self.requested_max_threads,
                    per_thread_limit,
                    total_system_memory(),
                )
            }
        };
        self.max_threads.store(resolved, Ordering::Release);
    }

    pub(crate) fn php_ini(&self) -> MutexGuard<'_, HashMap<String, String>> {
        lock_mutex(&self.php_ini)
    }
}

/// `phpmainthread.go:250-253` + `:262-278`, as one pure function of the
/// caller's request and the two `setAutomaticMaxThreads` inputs. An explicit
/// [`MaxThreads::Fixed`] value is never recomputed from `per_thread_limit`/
/// `total_memory` -- upstream returns from `setAutomaticMaxThreads` before
/// touching either. Either way, the result is then raised back up to
/// `num_threads` if it came out lower (`phpmainthread.go:251-252`), which
/// upstream applies unconditionally after `setAutomaticMaxThreads` returns,
/// not only on the automatic path.
///
/// No PHP or OS calls happen here: `per_thread_limit` and `total_memory` are
/// plain inputs, so this is unit-testable without booting PHP.
fn resolve_max_threads(
    num_threads: usize,
    requested: MaxThreads,
    per_thread_limit: i64,
    total_memory: u64,
) -> usize {
    let resolved = match requested {
        MaxThreads::Fixed(count) => count,
        MaxThreads::Auto => automatic_max_threads(num_threads, per_thread_limit, total_memory),
    };
    resolved.max(num_threads)
}

/// `phpmainthread.go:262-278` (`setAutomaticMaxThreads`'s body once `auto`
/// is already selected): `total_memory / per_thread_limit`, truncating, or
/// `num_threads * 2` when the interpreter has no real memory limit
/// (`per_thread_limit <= 0`, which includes `memory_limit = -1`, unlimited)
/// or the total system memory could not be determined (`total_memory ==
/// 0`). Both are the documented fallback, not an error path.
fn automatic_max_threads(num_threads: usize, per_thread_limit: i64, total_memory: u64) -> usize {
    if per_thread_limit <= 0 || total_memory == 0 {
        return num_threads.saturating_mul(2);
    }
    // per_thread_limit > 0 was just checked, so this widening is exact.
    (total_memory / (per_thread_limit as u64)) as usize
}

/// `internal/memory/memory_linux.go:5-13` (`TotalSysMemory`): `Totalram *
/// Unit` from `sysinfo(2)`, or `0` on any error -- a `0` routes
/// [`automatic_max_threads`] to the `num_threads * 2` fallback, same as
/// upstream. Deliberately not `/proc/meminfo`: a hardened container or
/// chroot without procfs would silently take the doubling fallback where
/// upstream still computes a real limit.
#[cfg(target_os = "linux")]
fn total_system_memory() -> u64 {
    // SAFETY: `libc::sysinfo` is `sysinfo(2)`: it takes a pointer to a
    // `libc::sysinfo` that it fully populates before returning 0, or leaves
    // untouched and returns -1 on error (`man 2 sysinfo`). Zeroing first
    // makes the error path well-defined too, since every field is a plain
    // integer with no invalid bit pattern. `info` is a local, exclusively
    // borrowed for the duration of this one call.
    let (status, info) = unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        (libc::sysinfo(&mut info), info)
    };
    if status != 0 {
        return 0;
    }
    // `totalram` is already `u64` (`c_ulong` on every Linux target this
    // project builds for is LP64); only `mem_unit` (`c_uint`) needs widening.
    info.totalram.wrapping_mul(u64::from(info.mem_unit))
}

/// `internal/memory/memory_others.go`: upstream has no non-Linux
/// implementation -- it is a hard `0`, which routes to the doubling
/// fallback by design, including on macOS.
#[cfg(not(target_os = "linux"))]
fn total_system_memory() -> u64 {
    0
}

#[derive(Default)]
struct Registry {
    main: Option<Arc<PhpMainThread>>,
    threads: Vec<Arc<PhpThread>>,
    metrics_initialized: bool,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// Upstream's `scalingMu`: excludes boot/handler conversion from a drain.
static SCALING: Mutex<()> = Mutex::new(());

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Starts PHP's main pthread and `num_threads` inactive PHP pthreads.
pub fn init_php_threads(
    num_threads: usize,
    max_threads: MaxThreads,
    php_ini: HashMap<String, String>,
) -> Result<Arc<PhpMainThread>, ThreadError> {
    let _scaling = lock_mutex(&SCALING);
    // `MaxThreads::Auto` cannot be validated up front -- its final value is
    // only known after `finalize_max_threads` resolves it, and that
    // resolution always yields at least `num_threads` (see
    // `resolve_max_threads`'s unconditional raise).
    let fixed_count_out_of_range = matches!(max_threads, MaxThreads::Fixed(count) if count == 0 || count > c_int::MAX as usize);
    if fixed_count_out_of_range || num_threads > c_int::MAX as usize {
        return Err(ThreadError::InvalidThreadCount {
            num_threads,
            max_threads,
        });
    }

    let main = Arc::new(PhpMainThread::new(num_threads, max_threads, php_ini));
    let initial_thread = Arc::new(PhpThread::new(0));
    {
        let mut installed = lock_mutex(registry());
        if installed.main.is_some() {
            return Err(ThreadError::AlreadyInitialized);
        }

        // Slot zero must precede the main pthread. Extensions can touch the
        // environment during module startup while C's thread index still has
        // its default value zero (`phpmainthread.go:53-58`).
        installed.main = Some(Arc::clone(&main));
        installed.threads = vec![Arc::clone(&initial_thread)];
        installed.metrics_initialized = false;
    }

    // SAFETY: the registry, main object, and slot zero are installed above. C
    // creates the main pthread and reports readiness through our blocking
    // callback; the `num_threads` conversion was checked above.
    let main_result = unsafe { frankenphp_new_main_thread(num_threads as c_int) };
    if main_result != 0 {
        if main_result != -1 {
            // pthread_create succeeded but detach failed. The pthread is live;
            // retain the registry until it reaches the same safe shutdown
            // point as a normally detached main thread.
            main.state.wait_for(&[State::Ready]);
            main.state.set(State::Done);
            main.state.wait_for(&[State::Reserved]);
        }
        clear_registry(&main);
        return Err(ThreadError::MainThreadCreation { code: main_result });
    }

    main.state.wait_for(&[State::Ready]);
    let final_max_threads = main.max_threads();

    // SAFETY: the main callback made `max_threads` final before publishing
    // Ready. This call is before every `frankenphp_new_php_thread`; C writes
    // `thread_metrics[thread_index]` unguarded at frankenphp.c:1541.
    unsafe { frankenphp_init_thread_metrics(final_max_threads as c_int) };

    let mut threads = Vec::with_capacity(final_max_threads);
    threads.push(initial_thread);
    for thread_index in 1..final_max_threads {
        threads.push(Arc::new(PhpThread::new(thread_index)));
    }
    {
        let mut installed = lock_mutex(registry());
        installed.threads.clone_from(&threads);
        installed.metrics_initialized = true;
    }

    // Booting sequentially needs no Rust-owned PHP threads: every launcher
    // call still creates the real pthread in C, then waits for its Inactive
    // callback rendezvous.
    for php_thread in threads.iter().take(num_threads) {
        if let Err(error) = php_thread.boot() {
            drain_generation(&main, &threads);
            clear_registry(&main);
            return Err(error);
        }
    }

    Ok(main)
}

/// Drains every PHP pthread before releasing the main pthread into TSRM
/// teardown.
pub fn drain_php_threads() {
    let _scaling = lock_mutex(&SCALING);
    let (main, threads) = {
        let installed = lock_mutex(registry());
        let Some(main) = installed.main.as_ref() else {
            return;
        };
        (Arc::clone(main), installed.threads.clone())
    };

    if !main.state.is(State::Ready) {
        return;
    }

    drain_generation(&main, &threads);
    clear_registry(&main);
}

fn drain_generation(main: &Arc<PhpMainThread>, threads: &[Arc<PhpThread>]) {
    main.state.set(State::ShuttingDown);

    // Request every shutdown before waiting for any one slot. This reproduces
    // upstream's concurrent shutdown goroutines without a fallible controller
    // thread spawn, and starts every grace period at the same point.
    let mut pending = Vec::with_capacity(threads.len());
    for php_thread in threads {
        if let Some(ticket) = php_thread.begin_shutdown() {
            pending.push((Arc::clone(php_thread), ticket));
        }
    }

    let grace_deadline = Instant::now() + SHUTDOWN_GRACE_PERIOD;
    for (php_thread, _) in &pending {
        if php_thread.state.is(State::Done) {
            continue;
        }
        let remaining = grace_deadline.saturating_duration_since(Instant::now());
        let _ = php_thread
            .state
            .wait_for_state_with_timeout(remaining, &[State::Done]);
    }

    // Once the shared grace deadline expires, arm every outstanding slot
    // before making any unbounded wait. A thread parked in a Rust receive was
    // already woken by its drain generation above; this signal covers Zend VM
    // execution and interruptible syscalls.
    for (php_thread, _) in &pending {
        if !php_thread.state.is(State::Done) {
            report_force_kill(php_thread);
            php_thread.send_kill_signal();
        }
    }
    for (php_thread, _) in &pending {
        if !php_thread.state.is(State::Done) {
            // Intentionally unbounded; see the matching comment in
            // `PhpThread::shutdown` and upstream phpmainthread.go:179-196.
            php_thread.state.wait_for(&[State::Done]);
        }
    }
    for (php_thread, ticket) in pending {
        php_thread.finish_shutdown(ticket);
    }

    report_php_threads_reserved_for_test();

    // `php_main` may call tsrm_shutdown immediately after this publication.
    // Every PHP pthread has therefore reached Done (after ts_free_thread) and
    // its Rust slot has returned to Reserved first.
    main.state.set(State::Done);
    main.state.wait_for(&[State::Reserved]);

    let destroy_metrics = {
        let mut installed = lock_mutex(registry());
        std::mem::take(&mut installed.metrics_initialized)
    };
    if destroy_metrics {
        // SAFETY: every PHP pthread exited before main Done, and the main
        // shutdown callback has now published Reserved after TSRM teardown. No
        // C thread can access the metrics array again.
        unsafe { frankenphp_destroy_thread_metrics() };
    }
}

fn clear_registry(main: &Arc<PhpMainThread>) {
    let mut installed = lock_mutex(registry());
    if installed
        .main
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, main))
    {
        installed.main = None;
        installed.threads.clear();
        installed.metrics_initialized = false;
    }
}

/// Exclusive claim on an inactive PHP-thread slot.
///
/// The claim retains upstream's `scalingMu` equivalent from slot selection
/// through handler assignment. It exposes no thread handle before assignment,
/// so two safe concurrent allocators cannot both select the same inactive
/// slot. Dropping an unused claim simply makes the still-inactive slot
/// selectable again.
pub struct InactivePhpThreadClaim {
    thread: Arc<PhpThread>,
    scaling: MutexGuard<'static, ()>,
}

impl InactivePhpThreadClaim {
    /// Installs `handler` while the selection lock remains held.
    pub fn assign_handler(self, handler: Arc<dyn ThreadHandler>) -> Option<Arc<PhpThread>> {
        let Self { thread, scaling } = self;
        let assigned = thread.set_handler_locked(handler);
        drop(scaling);
        assigned.then_some(thread)
    }
}

/// Claims an inactive slot, booting one reserved slot when capacity remains.
/// The returned token keeps the scan/C-launch operation and the subsequent
/// handler assignment atomic with respect to other allocators and
/// `drain_php_threads`.
pub fn get_inactive_php_thread() -> Option<InactivePhpThreadClaim> {
    let scaling = lock_mutex(&SCALING);
    let threads = php_threads();

    if let Some(thread) = threads
        .iter()
        .find(|thread| thread.state.is(State::Inactive))
    {
        return Some(InactivePhpThreadClaim {
            thread: Arc::clone(thread),
            scaling,
        });
    }

    for thread in threads {
        if thread
            .state
            .compare_and_swap(State::Reserved, State::BootRequested)
        {
            return thread
                .boot()
                .ok()
                .map(|()| InactivePhpThreadClaim { thread, scaling });
        }
    }
    None
}

pub fn convert_to_inactive_thread(thread: &PhpThread) -> bool {
    thread.set_handler(Arc::new(InactiveThread))
}

pub fn php_threads() -> Vec<Arc<PhpThread>> {
    lock_mutex(registry()).threads.clone()
}

pub fn main_thread() -> Option<Arc<PhpMainThread>> {
    lock_mutex(registry()).main.clone()
}

pub(crate) fn thread_by_index(thread_index: usize) -> Option<Arc<PhpThread>> {
    lock_mutex(registry()).threads.get(thread_index).cloned()
}

fn report_force_kill(thread: &PhpThread) {
    write_diagnostic(format_args!(
        "force-killing thread {} after {:?} in state {}",
        thread.index(),
        SHUTDOWN_GRACE_PERIOD,
        thread.state().name()
    ));
}

fn write_diagnostic(arguments: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "frankenrust: {arguments}");
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read_rwlock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_rwlock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
const LIFECYCLE_CHILD_MARKER: &str = "FRANKENRUST_LIFECYCLE_CHILD";

#[cfg(test)]
fn report_php_threads_reserved_for_test() {
    if std::env::var_os(LIFECYCLE_CHILD_MARKER).is_some() {
        write_test_marker("PHP_THREADS_RESERVED");
    }
}

#[cfg(not(test))]
fn report_php_threads_reserved_for_test() {}

#[cfg(test)]
pub(crate) fn report_main_callback_return_for_test() {
    if std::env::var_os(LIFECYCLE_CHILD_MARKER).is_some() {
        write_test_marker("MAIN_READY_CALLBACK_RETURNING");
    }
}

#[cfg(not(test))]
pub(crate) fn report_main_callback_return_for_test() {}

#[cfg(test)]
fn write_test_marker(marker: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{marker}");
    let _ = stdout.flush();
}

#[cfg(test)]
pub(crate) static TEST_REGISTRY: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn install_test_main(php_ini: HashMap<String, String>) -> Arc<PhpMainThread> {
    let main = Arc::new(PhpMainThread::new(0, MaxThreads::Fixed(1), php_ini));
    let mut installed = lock_mutex(registry());
    if installed.main.is_none() {
        installed.main = Some(Arc::clone(&main));
    }
    main
}

#[cfg(test)]
pub(crate) fn remove_test_main(main: &Arc<PhpMainThread>) {
    clear_registry(main);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::sync::mpsc;

    const LIFECYCLE_TEST: &str =
        "thread::tests::one_shot_real_boot_and_drain_reaches_the_known_output_stub";
    const SIGABRT: i32 = 6;

    struct StandInLauncher;

    impl PhpThreadLauncher for StandInLauncher {
        fn launch(&self, thread: Arc<PhpThread>) -> bool {
            std::thread::Builder::new()
                .name(format!("php-stand-in-{}", thread.index()))
                .spawn(move || loop {
                    match thread.before_script_execution() {
                        Some(_) => thread.after_script_execution(0),
                        None => {
                            thread.on_thread_shutdown();
                            return;
                        }
                    }
                })
                .is_ok()
        }
    }

    struct ParkingHandler;

    impl ThreadHandler for ParkingHandler {
        fn name(&self) -> &str {
            "Parking Test Thread"
        }

        fn before_script_execution(&self, thread: &PhpThread) -> Option<ScriptPath> {
            loop {
                match thread.state_machine().get() {
                    State::TransitionRequested => return thread.transition_to_new_handler(),
                    State::TransitionComplete | State::Inactive => {
                        thread.state_machine().set(State::Inactive);
                        thread.state_machine().mark_as_waiting(true);
                        let _ = thread.drain_receiver().receiver().recv();
                        thread.state_machine().mark_as_waiting(false);
                    }
                    State::ShuttingDown | State::Rebooting | State::ForceRebooting => return None,
                    _ => thread.state_machine().wait_for(&[
                        State::TransitionRequested,
                        State::TransitionComplete,
                        State::Inactive,
                        State::ShuttingDown,
                        State::Rebooting,
                        State::ForceRebooting,
                    ]),
                }
            }
        }

        fn after_script_execution(&self, _thread: &PhpThread, _exit_status: i32) {}
    }

    struct ClaimedHandler;

    impl ThreadHandler for ClaimedHandler {
        fn name(&self) -> &str {
            "Claimed Test Thread"
        }

        fn before_script_execution(&self, thread: &PhpThread) -> Option<ScriptPath> {
            loop {
                match thread.state_machine().get() {
                    State::TransitionRequested => return thread.transition_to_new_handler(),
                    State::TransitionComplete => thread.state_machine().set(State::Ready),
                    State::Ready => {
                        thread.state_machine().mark_as_waiting(true);
                        let _ = thread.drain_receiver().receiver().recv();
                        thread.state_machine().mark_as_waiting(false);
                    }
                    State::ShuttingDown | State::Rebooting | State::ForceRebooting => return None,
                    _ => thread.state_machine().wait_for(&[
                        State::TransitionRequested,
                        State::TransitionComplete,
                        State::Ready,
                        State::ShuttingDown,
                        State::Rebooting,
                        State::ForceRebooting,
                    ]),
                }
            }
        }

        fn after_script_execution(&self, _thread: &PhpThread, _exit_status: i32) {}
    }

    fn stand_in_thread(index: usize) -> Arc<PhpThread> {
        Arc::new(PhpThread::with_launcher(index, Arc::new(StandInLauncher)))
    }

    #[test]
    fn handler_value_lock_excludes_replacement_during_a_read() {
        let slot = Arc::new(HandlerSlot::new());
        let writer = slot.writer();
        slot.replace(&writer, Arc::new(ParkingHandler));
        drop(writer);

        let writer_slot = Arc::clone(&slot);
        let value_reader = read_rwlock(&slot.value);
        let (replaced_sender, replaced_receiver) = mpsc::channel();
        let replacer = std::thread::spawn(move || {
            let writer = writer_slot.writer();
            writer_slot.replace(&writer, Arc::new(ClaimedHandler));
            let _ = replaced_sender.send(());
        });

        assert_eq!(
            replaced_receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "handler replacement must wait for an active value reader"
        );
        drop(value_reader);
        assert_eq!(
            replaced_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(()),
            "handler replacement did not resume after the reader released"
        );
        replacer.join().expect("handler replacer must not panic");

        let writer = slot.writer();
        assert_eq!(
            slot.read_with_writer(&writer)
                .expect("replacement handler is installed")
                .name(),
            "Claimed Test Thread"
        );
    }

    #[test]
    fn inactive_slot_claim_is_held_through_handler_assignment() {
        let _serial = TEST_REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
        let thread = stand_in_thread(0);
        thread.boot().expect("stand-in boot must succeed");
        {
            let mut installed = lock_mutex(registry());
            assert!(installed.main.is_none());
            assert!(installed.threads.is_empty());
            installed.threads.push(Arc::clone(&thread));
        }

        let (claimed_sender, claimed_receiver) = mpsc::channel();
        let (assign_sender, assign_receiver) = mpsc::channel();
        let (assigned_sender, assigned_receiver) = mpsc::channel();
        let first_allocator = std::thread::spawn(move || {
            let claim = get_inactive_php_thread().expect("inactive slot should be claimable");
            let _ = claimed_sender.send(());
            let _ = assign_receiver.recv();
            let assigned = claim.assign_handler(Arc::new(ClaimedHandler));
            let _ = assigned_sender.send(assigned);
        });

        assert_eq!(
            claimed_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(()),
            "first allocator did not claim the inactive slot"
        );

        let (second_sender, second_receiver) = mpsc::channel();
        let second_allocator = std::thread::spawn(move || {
            let second_claim = get_inactive_php_thread();
            let _ = second_sender.send(second_claim.is_some());
        });

        let second_before_assignment = second_receiver.recv_timeout(Duration::from_millis(50));
        let _ = assign_sender.send(());
        let assigned = assigned_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("first allocator did not finish handler assignment")
            .expect("first allocator lost its claimed slot");
        let second_claimed_same_slot = match second_before_assignment {
            Ok(claimed) => claimed,
            Err(mpsc::RecvTimeoutError::Timeout) => second_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("second allocator remained blocked after assignment"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("second allocator exited without reporting its result")
            }
        };

        first_allocator
            .join()
            .expect("first allocator must not panic");
        second_allocator
            .join()
            .expect("second allocator must not panic");
        assert_eq!(
            second_before_assignment,
            Err(mpsc::RecvTimeoutError::Timeout),
            "a second allocator passed selection before the first assigned its handler"
        );
        assert!(
            !second_claimed_same_slot,
            "the assigned slot must not be returned to a second allocator"
        );
        assert_eq!(assigned.name(), "Claimed Test Thread");

        assigned.shutdown();
        lock_mutex(registry()).threads.clear();
    }

    #[test]
    fn per_slot_boot_transition_shutdown_repeats_three_times() {
        let thread = stand_in_thread(0);

        for cycle in 0..3 {
            thread
                .boot()
                .unwrap_or_else(|error| panic!("cycle {cycle}: stand-in boot failed: {error}"));
            assert_eq!(thread.state(), State::Inactive);

            assert!(thread.set_handler(Arc::new(ParkingHandler)));
            thread.state_machine().wait_for(&[State::Inactive]);
            assert_eq!(thread.state(), State::Inactive);
            assert_eq!(thread.name(), "Parking Test Thread");

            thread.shutdown();
            assert!(
                thread.state() == State::Reserved,
                "cycle {cycle}: slot did not return to Reserved"
            );
        }
    }

    #[test]
    fn drain_receivers_are_isolated_by_generation() {
        let thread = stand_in_thread(1);
        thread.boot().expect("stand-in boot must succeed");

        let before_transition = thread.drain_receiver();
        assert!(thread.set_handler(Arc::new(ParkingHandler)));
        thread.state_machine().wait_for(&[State::Inactive]);

        assert_eq!(
            before_transition.receiver().try_recv(),
            Err(TryRecvError::Disconnected),
            "set_handler must close the previous generation"
        );
        let after_transition = thread.drain_receiver();
        assert_ne!(
            after_transition.generation(),
            before_transition.generation()
        );
        assert_eq!(
            after_transition.receiver().try_recv(),
            Err(TryRecvError::Empty),
            "the replacement handler must not inherit the old close"
        );

        let before_shutdown = after_transition.clone();
        thread.shutdown();
        assert_eq!(
            before_shutdown.receiver().try_recv(),
            Err(TryRecvError::Disconnected),
            "shutdown must close the receiver captured before it"
        );
        let after_shutdown = thread.drain_receiver();
        assert_ne!(after_shutdown.generation(), before_shutdown.generation());
        assert_eq!(
            after_shutdown.receiver().try_recv(),
            Err(TryRecvError::Empty)
        );
    }

    #[test]
    fn drain_channel_receiver_survives_the_close_reopen_window() {
        // Regression test for #10's blocking review finding: `DrainChannel`
        // used to store the receiver inside the same `Option` that gated the
        // sender, so a receiver fetched between `close()` and the matching
        // `reopen()` was `None` — the exact window during which the close is
        // the wake signal a parked handler relies on. Drive `close`/`reopen`
        // directly, without a handler, to sample strictly inside that window.
        let drain = DrainChannel::new();
        let generation_before_close = drain.receiver().generation();

        let closed = drain
            .close()
            .expect("a fresh generation has an open sender to close");

        // Sampled while only the sender has been taken: must still be a live
        // receiver whose disconnect is observable, never a missing one.
        let receiver_during_window = drain.receiver();
        assert_eq!(receiver_during_window.generation(), generation_before_close);
        assert_eq!(
            receiver_during_window.receiver().try_recv(),
            Err(TryRecvError::Disconnected),
            "a receiver fetched inside the close/reopen window must observe the close"
        );

        drain.reopen(closed);
        let receiver_after_reopen = drain.receiver();
        assert_ne!(
            receiver_after_reopen.generation(),
            generation_before_close,
            "reopen must start a strictly newer generation"
        );
        assert_eq!(
            receiver_after_reopen.receiver().try_recv(),
            Err(TryRecvError::Empty),
            "the reopened generation must not inherit the previous close"
        );
    }

    #[test]
    fn drain_channel_close_is_one_shot_per_generation() {
        // "Closing twice within one generation is impossible by
        // construction": `close` takes `Option<Sender<()>>::take`, so a
        // second call on the same still-open generation observes `None`
        // rather than closing an already-closed channel (which panics for
        // Go's `close`, the construction this mirrors).
        let drain = DrainChannel::new();
        assert!(drain.close().is_some());
        assert!(
            drain.close().is_none(),
            "a second close before reopen must find no sender left to take"
        );
    }

    #[test]
    fn one_shot_real_boot_and_drain_reaches_the_known_output_stub() {
        if std::env::var_os(LIFECYCLE_CHILD_MARKER).is_some() {
            run_real_lifecycle_child();
            return;
        }

        let executable =
            std::env::current_exe().expect("current_exe should resolve for a test binary");
        let output = Command::new(&executable)
            .args(["--exact", "--nocapture", LIFECYCLE_TEST])
            .env(LIFECYCLE_CHILD_MARKER, "1")
            .output()
            .unwrap_or_else(|error| panic!("failed to re-run {}: {error}", executable.display()));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        for marker in [
            "PHP_THREADS_INACTIVE",
            "PHP_THREADS_RESERVED",
            "MAIN_READY_CALLBACK_RETURNING",
        ] {
            assert!(
                stdout.contains(marker),
                "child never reported {marker}; status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                output.status
            );
        }
        assert_eq!(
            output.status.signal(),
            Some(SIGABRT),
            "the child must currently die in #12's output stub; status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stderr.contains("go_write_headers"),
            "the child died somewhere other than the named output stub:\n{stderr}"
        );
        assert!(
            !stdout.contains("UNEXPECTED_DRAIN_RETURN"),
            "the test must fail loudly once the abort stub stops firing"
        );
    }

    fn run_real_lifecycle_child() {
        let php_ini = HashMap::from([
            ("display_errors".to_string(), "0".to_string()),
            ("display_startup_errors".to_string(), "0".to_string()),
            ("log_errors".to_string(), "0".to_string()),
        ]);
        let main = init_php_threads(2, MaxThreads::Fixed(2), php_ini)
            .expect("real lifecycle boot must succeed");
        let threads = php_threads();
        assert_eq!(threads.len(), 2);
        assert!(threads
            .iter()
            .all(|thread| thread.state() == State::Inactive));
        write_test_marker("PHP_THREADS_INACTIVE");

        drain_php_threads();

        // #12 replacing go_write_headers makes this reachable. This issue's
        // subprocess assertion intentionally fails then; #97 owns the positive
        // post-tsrm_shutdown lifecycle.
        assert_eq!(main.state(), State::Reserved);
        write_test_marker("UNEXPECTED_DRAIN_RETURN");
    }

    // #103: max_threads=auto resolution (phpmainthread.go:250-278). Pure
    // functions of (num_threads, per_thread_limit, total_memory) -- no PHP
    // or OS calls, so these run without booting PHP.

    #[test]
    fn explicit_max_threads_passes_through_untouched() {
        // Garbage per_thread_limit/total_memory: an explicit value must
        // never be recomputed from them (phpmainthread.go:263-265's early
        // return).
        assert_eq!(
            resolve_max_threads(2, MaxThreads::Fixed(10), -1, 0),
            10,
            "an explicit max_threads at or above num_threads must pass through unchanged"
        );
    }

    #[test]
    fn explicit_max_threads_below_num_threads_is_still_raised() {
        // phpmainthread.go:251-252's raise runs unconditionally after
        // setAutomaticMaxThreads returns, on both the explicit and the
        // automatic path.
        assert_eq!(
            resolve_max_threads(10, MaxThreads::Fixed(3), -1, 0),
            10,
            "an explicit max_threads below num_threads must be raised to it"
        );
    }

    #[test]
    fn automatic_max_threads_doubles_when_there_is_no_real_memory_limit() {
        assert_eq!(
            automatic_max_threads(4, 0, 1024),
            8,
            "memory_limit=0 must double num_threads"
        );
        assert_eq!(
            automatic_max_threads(4, -1, 1024),
            8,
            "memory_limit=-1 (unlimited) must double num_threads"
        );
    }

    #[test]
    fn automatic_max_threads_doubles_when_total_memory_is_unknown() {
        assert_eq!(
            automatic_max_threads(4, 128 * 1024 * 1024, 0),
            8,
            "an undetermined total system memory (0) must double num_threads"
        );
    }

    #[test]
    fn automatic_max_threads_divides_and_truncates() {
        // 1000 / 300 = 3.33..., must truncate to 3, not round.
        assert_eq!(automatic_max_threads(1, 300, 1000), 3);
        // Exact division still works.
        assert_eq!(
            automatic_max_threads(1, 128 * 1024 * 1024, 1024 * 1024 * 1024),
            8
        );
    }

    #[test]
    fn resolve_max_threads_raises_a_low_automatic_result_to_num_threads() {
        // total / per_thread_limit truncates to 0, well below num_threads.
        assert_eq!(
            resolve_max_threads(10, MaxThreads::Auto, 1_000_000, 500_000),
            10,
            "an automatic result below num_threads must be raised to it"
        );
    }

    #[test]
    fn resolve_max_threads_auto_uses_the_automatic_result_when_it_is_higher() {
        assert_eq!(
            resolve_max_threads(1, MaxThreads::Auto, 128 * 1024 * 1024, 1024 * 1024 * 1024),
            8
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn total_system_memory_is_nonzero_on_linux() {
        assert_ne!(
            total_system_memory(),
            0,
            "sysinfo(2) must resolve a real total on a normal Linux host"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn total_system_memory_is_exactly_zero_off_linux() {
        // memory_others.go: no non-Linux implementation exists upstream, so
        // this must route callers to the doubling fallback by design.
        assert_eq!(total_system_memory(), 0);
    }
}
