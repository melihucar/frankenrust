//! Thread lifecycle state and blocking state-change subscriptions.

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A PHP thread lifecycle state.
///
/// The discriminants mirror `internal/state/state.go` and are part of the
/// state-machine contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum State {
    Reserved = 0,
    Booting = 1,
    BootRequested = 2,
    ShuttingDown = 3,
    Done = 4,
    Inactive = 5,
    Ready = 6,
    TransitionRequested = 7,
    TransitionInProgress = 8,
    TransitionComplete = 9,
    Rebooting = 10,
    ForceRebooting = 11,
    RebootReady = 12,
    YieldingForReboot = 13,
}

impl State {
    /// Returns the log and panic-message name used by upstream FrankenPHP.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Booting => "booting",
            Self::BootRequested => "boot requested",
            Self::ShuttingDown => "shutting down",
            Self::Done => "done",
            Self::Inactive => "inactive",
            Self::Ready => "ready",
            Self::TransitionRequested => "transition requested",
            Self::TransitionInProgress => "transition in progress",
            Self::TransitionComplete => "transition complete",
            Self::Rebooting => "rebooting",
            Self::ForceRebooting => "rebooting (force)",
            Self::RebootReady => "reboot ready",
            Self::YieldingForReboot => "yielding for reboot",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

struct StateSubscriber {
    states: Vec<State>,
    sender: Option<Sender<()>>,
    token: Arc<()>,
}

struct StateInner {
    current_state: State,
    subscribers: Vec<StateSubscriber>,
}

/// Synchronizes a PHP thread's lifecycle state with its controllers.
pub struct ThreadState {
    inner: Mutex<StateInner>,
    /// Unix milliseconds, with zero meaning that the thread is not waiting.
    waiting_since: AtomicI64,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadState {
    /// Creates a state machine in [`State::Reserved`].
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StateInner {
                current_state: State::Reserved,
                subscribers: Vec::new(),
            }),
            waiting_since: AtomicI64::new(0),
        }
    }

    /// Returns whether the current state equals `state`.
    pub fn is(&self, state: State) -> bool {
        self.lock().current_state == state
    }

    /// Changes `compare_to` to `swap_to`, notifying subscribers on success.
    pub fn compare_and_swap(&self, compare_to: State, swap_to: State) -> bool {
        let mut inner = self.lock();
        if inner.current_state != compare_to {
            return false;
        }

        inner.current_state = swap_to;
        Self::notify_subscribers(&mut inner, swap_to);
        true
    }

    /// Returns the current state's log and panic-message name.
    pub fn name(&self) -> &'static str {
        self.get().name()
    }

    /// Returns the current state.
    pub fn get(&self) -> State {
        self.lock().current_state
    }

    /// Overwrites the current state and notifies matching subscribers.
    ///
    /// As upstream does, this intentionally performs no transition validation
    /// and notifies even when `next_state` equals the current state.
    pub fn set(&self, next_state: State) {
        let mut inner = self.lock();
        inner.current_state = next_state;
        Self::notify_subscribers(&mut inner, next_state);
    }

    /// Blocks until the thread reaches any of `states`.
    pub fn wait_for(&self, states: &[State]) {
        let Some((receiver, _token)) = self.subscribe(states) else {
            return;
        };

        match receiver.recv() {
            Err(RecvError) => {}
            Ok(()) => unreachable!("state subscribers are notified by closing their channel"),
        }
    }

    /// Blocks until a requested state is reached or `timeout` elapses.
    pub fn wait_for_state_with_timeout(&self, timeout: Duration, states: &[State]) -> bool {
        let Some((receiver, token)) = self.subscribe(states) else {
            return true;
        };

        match receiver.recv_timeout(timeout) {
            Err(RecvTimeoutError::Disconnected) => true,
            Err(RecvTimeoutError::Timeout) => {
                let mut inner = self.lock();
                inner
                    .subscribers
                    .retain(|subscriber| !Arc::ptr_eq(&subscriber.token, &token));
                false
            }
            Ok(()) => unreachable!("state subscribers are notified by closing their channel"),
        }
    }

    /// Requests a state change once the thread is in a safe, stable state.
    pub fn request_safe_state_change(&self, next_state: State) -> bool {
        loop {
            {
                let mut inner = self.lock();
                match inner.current_state {
                    State::Reserved | State::ShuttingDown | State::Done => return false,
                    State::Ready | State::Inactive => {
                        inner.current_state = next_state;
                        Self::notify_subscribers(&mut inner, next_state);
                        return true;
                    }
                    _ => {}
                }
            }

            // Reserved is deliberately a wake condition but is rejected on
            // the next iteration, matching upstream's asymmetric behavior.
            self.wait_for(&[State::Ready, State::Inactive, State::Reserved]);
        }
    }

    /// Records whether the thread is waiting for a request or shutdown.
    pub fn mark_as_waiting(&self, is_waiting: bool) {
        let waiting_since = if is_waiting {
            unix_millis(SystemTime::now())
        } else {
            0
        };
        self.waiting_since.store(waiting_since, Ordering::SeqCst);
    }

    /// Returns whether the thread has a nonzero waiting timestamp.
    pub fn is_in_waiting_state(&self) -> bool {
        self.waiting_since.load(Ordering::SeqCst) != 0
    }

    /// Returns how many milliseconds have elapsed since waiting began.
    pub fn wait_time(&self) -> i64 {
        let since = self.waiting_since.load(Ordering::SeqCst);
        if since == 0 {
            return 0;
        }

        // Go's `int64` subtraction also wraps on overflow. Make that behavior
        // explicit so debug and release builds agree with upstream.
        unix_millis(SystemTime::now()).wrapping_sub(since)
    }

    /// Replaces the waiting timestamp with `time`'s Unix-millisecond value.
    pub fn set_wait_time(&self, time: SystemTime) {
        self.waiting_since
            .store(unix_millis(time), Ordering::SeqCst);
    }

    /// Takes the state lock, deliberately ignoring poisoning.
    ///
    /// Go's `sync.RWMutex` has no poisoning, so upstream has no equivalent of
    /// this decision and the divergence is ours to justify: this lock is taken
    /// from every PHP pthread, and propagating a panic on one thread into every
    /// other thread's `lock()` would turn one dead request into a dead process.
    /// The guarded data is two plain fields with no cross-field invariant a
    /// panic could tear, so continuing on the recovered state is sound.
    fn lock(&self) -> MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Checks the current state and subscribes while holding one lock guard.
    fn subscribe(&self, states: &[State]) -> Option<(Receiver<()>, Arc<()>)> {
        let mut inner = self.lock();
        if states.contains(&inner.current_state) {
            return None;
        }

        let (sender, receiver) = mpsc::channel();
        let token = Arc::new(());
        inner.subscribers.push(StateSubscriber {
            states: states.to_vec(),
            sender: Some(sender),
            token: Arc::clone(&token),
        });
        Some((receiver, token))
    }

    /// Notifies and removes every subscriber interested in `next_state`.
    ///
    /// Callers hold `inner`'s mutex, just like upstream's write-locked
    /// `notifySubscribers`. Taking and dropping the only sender closes the
    /// channel without blocking while the lock is held.
    fn notify_subscribers(inner: &mut StateInner, next_state: State) {
        inner.subscribers.retain_mut(|subscriber| {
            if subscriber.states.contains(&next_state) {
                drop(subscriber.sender.take());
                false
            } else {
                true
            }
        });
    }
}

fn unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => {
            let duration = error.duration();
            let magnitude = duration
                .as_millis()
                .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0));
            i64::try_from(magnitude).map_or(i64::MIN, |millis| -millis)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Barrier};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const STRESS_TIMEOUT: Duration = Duration::from_secs(10);
    const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn every_state_has_the_upstream_discriminant_and_name() {
        let _watchdog = TestWatchdog::start();
        let cases = [
            (State::Reserved, 0, "reserved"),
            (State::Booting, 1, "booting"),
            (State::BootRequested, 2, "boot requested"),
            (State::ShuttingDown, 3, "shutting down"),
            (State::Done, 4, "done"),
            (State::Inactive, 5, "inactive"),
            (State::Ready, 6, "ready"),
            (State::TransitionRequested, 7, "transition requested"),
            (State::TransitionInProgress, 8, "transition in progress"),
            (State::TransitionComplete, 9, "transition complete"),
            (State::Rebooting, 10, "rebooting"),
            (State::ForceRebooting, 11, "rebooting (force)"),
            (State::RebootReady, 12, "reboot ready"),
            (State::YieldingForReboot, 13, "yielding for reboot"),
        ];
        let state = ThreadState::new();

        for (value, discriminant, name) in cases {
            state.set(value);
            assert_eq!(value as u8, discriminant);
            assert_eq!(value.name(), name);
            assert_eq!(value.to_string(), name);
            assert_eq!(state.name(), name);
        }
    }

    #[test]
    fn compare_and_swap_changes_only_a_matching_state() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for(&[State::Ready]);
        });
        wait_for_subscriber_count(&state, 1, Instant::now() + TEST_TIMEOUT);

        assert!(!state.compare_and_swap(State::Booting, State::Ready));
        assert_eq!(state.get(), State::Reserved);
        assert_eq!(state.lock().subscribers.len(), 1);
        assert!(state.compare_and_swap(State::Reserved, State::Ready));
        join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
        assert_eq!(state.get(), State::Ready);
    }

    #[test]
    fn wait_for_returns_immediately_for_an_existing_target_state() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Ready);
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for(&[State::Inactive, State::Ready]);
        });

        join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
        assert!(state.lock().subscribers.is_empty());
    }

    #[test]
    fn wait_for_is_woken_by_set_from_another_thread() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for(&[State::Ready]);
        });
        wait_for_subscriber_count(&state, 1, Instant::now() + TEST_TIMEOUT);

        state.set(State::Ready);

        join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
        assert!(state.is(State::Ready));
    }

    #[test]
    fn one_transition_wakes_every_matching_waiter() {
        const WAITER_COUNT: usize = 8;

        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        let mut workers = Vec::new();
        for _ in 0..WAITER_COUNT {
            let worker_state = Arc::clone(&state);
            workers.push(spawn_bounded(move || {
                worker_state.wait_for(&[State::Inactive]);
            }));
        }
        let deadline = Instant::now() + TEST_TIMEOUT;
        wait_for_subscriber_count(&state, WAITER_COUNT, deadline);

        state.set(State::Inactive);

        for (worker, finished) in workers {
            join_by_deadline(worker, finished, deadline);
        }
        assert!(state.lock().subscribers.is_empty());
    }

    #[test]
    fn waiter_for_two_states_ignores_other_transitions() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Booting);
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for(&[State::Ready, State::Inactive]);
        });
        wait_for_subscriber_count(&state, 1, Instant::now() + TEST_TIMEOUT);

        state.set(State::TransitionComplete);
        assert_eq!(
            finished.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        assert_eq!(state.lock().subscribers.len(), 1);

        state.set(State::Inactive);
        join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
    }

    /// Upstream's `TestStateShouldHaveCorrectAmountOfSubscribers`
    /// (`internal/state/state_test.go:24-38`). A transition that wakes some
    /// subscribers while keeping others is what exercises the compaction in
    /// `notify_subscribers`; a set of identical waiters cannot reach it.
    #[test]
    fn overlapping_subscribers_are_compacted_like_upstream() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Booting);

        let waiters: Vec<_> = [
            vec![State::Inactive],
            vec![State::Inactive, State::ShuttingDown],
            vec![State::ShuttingDown],
        ]
        .into_iter()
        .map(|states| {
            let worker_state = Arc::clone(&state);
            spawn_bounded(move || worker_state.wait_for(&states))
        })
        .collect();

        let deadline = Instant::now() + TEST_TIMEOUT;
        wait_for_subscriber_count(&state, 3, deadline);

        // Removes both subscribers interested in Inactive and keeps the sole
        // ShuttingDown-only subscriber. Subscription order races between the
        // threads, so the surviving entry may occupy any list position; the
        // post-transition counts verify compaction independent of that order.
        // `notify_subscribers` runs under the lock, so the counts below are
        // settled by the time the call returns and need no polling.
        state.set(State::Inactive);
        assert_eq!(state.lock().subscribers.len(), 1);

        assert!(state.compare_and_swap(State::Inactive, State::ShuttingDown));
        assert_eq!(state.lock().subscribers.len(), 0);

        for (worker, finished) in waiters {
            join_by_deadline(worker, finished, deadline);
        }
    }

    /// Upstream's `Test2GoroutinesYieldToEachOtherViaStates`
    /// (`internal/state/state_test.go:10-22`). A full round trip in which each
    /// side both wakes the other and is woken by it: a lost wakeup in either
    /// direction hangs this, which is how it would present in #10.
    #[test]
    fn two_threads_yield_to_each_other_via_states() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Booting);

        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for(&[State::Inactive]);
            assert!(worker_state.is(State::Inactive));
            worker_state.set(State::Ready);
        });

        state.set(State::Inactive);
        state.wait_for(&[State::Ready]);
        assert!(state.is(State::Ready));

        join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
    }

    #[test]
    fn timed_out_waiter_removes_its_subscriber() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.wait_for_state_with_timeout(Duration::from_millis(25), &[State::Ready])
        });

        let reached_state = join_by_deadline(worker, finished, Instant::now() + TEST_TIMEOUT);
        assert!(!reached_state);
        assert!(state.lock().subscribers.is_empty());
    }

    #[test]
    fn safe_state_change_rejects_done_and_accepts_ready() {
        let _watchdog = TestWatchdog::start();
        let state = ThreadState::new();
        state.set(State::Done);
        assert!(!state.request_safe_state_change(State::Rebooting));
        assert_eq!(state.get(), State::Done);

        state.set(State::Ready);
        assert!(state.request_safe_state_change(State::TransitionRequested));
        assert_eq!(state.get(), State::TransitionRequested);
    }

    #[test]
    fn safe_state_change_waits_until_the_state_is_stable() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Booting);
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.request_safe_state_change(State::TransitionRequested)
        });
        wait_for_subscriber_count(&state, 1, Instant::now() + TEST_TIMEOUT);

        state.set(State::Ready);

        assert!(join_by_deadline(
            worker,
            finished,
            Instant::now() + TEST_TIMEOUT
        ));
        assert_eq!(state.get(), State::TransitionRequested);
    }

    #[test]
    fn safe_state_change_wakes_for_reserved_then_rejects_it() {
        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        state.set(State::Booting);
        let worker_state = Arc::clone(&state);
        let (worker, finished) = spawn_bounded(move || {
            worker_state.request_safe_state_change(State::TransitionRequested)
        });
        wait_for_subscriber_count(&state, 1, Instant::now() + TEST_TIMEOUT);

        state.set(State::Reserved);

        assert!(!join_by_deadline(
            worker,
            finished,
            Instant::now() + TEST_TIMEOUT
        ));
        assert_eq!(state.get(), State::Reserved);
    }

    #[test]
    fn setting_the_same_value_still_notifies_subscribers() {
        let _watchdog = TestWatchdog::start();
        let state = ThreadState::new();
        let (receiver, _token) = state
            .subscribe(&[State::Ready])
            .expect("reserved is not a target state");
        state.lock().current_state = State::Ready;

        state.set(State::Ready);

        assert_eq!(
            receiver.recv_timeout(TEST_TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        );
        assert!(state.lock().subscribers.is_empty());
    }

    #[test]
    fn waiting_timestamp_preserves_elapsed_and_future_signs() {
        let _watchdog = TestWatchdog::start();
        let state = ThreadState::new();
        assert!(!state.is_in_waiting_state());
        assert_eq!(state.wait_time(), 0);

        state.mark_as_waiting(true);
        assert!(state.is_in_waiting_state());
        state.mark_as_waiting(false);
        assert!(!state.is_in_waiting_state());
        assert_eq!(state.wait_time(), 0);

        // `set_wait_time` stores one clock reading and `wait_time` takes
        // another, so the delta between them is non-negative and can only push
        // the result *upwards*. Both offsets shift by a whole number of
        // milliseconds, so `unix_millis`' truncation cancels: the lower bounds
        // below are exact, not approximate, and only the upper bounds absorb
        // scheduling slop. Give both directions the same headroom -- an
        // asymmetric bound makes one half of this test the first thing to flake
        // on an oversubscribed CI box.
        const OFFSET_MS: i64 = 2_000;
        const SLOP_MS: i64 = 1_000;
        let offset = Duration::from_millis(OFFSET_MS.unsigned_abs());

        state.set_wait_time(SystemTime::now() - offset);
        let elapsed = state.wait_time();
        assert!(
            (OFFSET_MS..=OFFSET_MS + SLOP_MS).contains(&elapsed),
            "a past instant must read back as a wait of at least the offset, got {elapsed}"
        );

        state.set_wait_time(SystemTime::now() + offset);
        let remaining = state.wait_time();
        assert!(
            (-OFFSET_MS..=-OFFSET_MS + SLOP_MS).contains(&remaining),
            "a future instant must yield a negative wait time, as upstream's does, got {remaining}"
        );

        state.set_wait_time(UNIX_EPOCH - Duration::from_micros(500));
        assert_eq!(state.waiting_since.load(Ordering::SeqCst), -1);
    }

    #[test]
    fn concurrent_set_and_wait_for_stress_completes_without_deadlock() {
        const WAITER_COUNT: usize = 6;
        const ROUNDS: usize = 250;

        let _watchdog = TestWatchdog::start();
        let state = Arc::new(ThreadState::new());
        let start_round = Arc::new(Barrier::new(WAITER_COUNT + 1));
        let end_round = Arc::new(Barrier::new(WAITER_COUNT + 1));
        let (done_sender, done_receiver) = mpsc::channel();
        let mut waiters = Vec::new();

        for _ in 0..WAITER_COUNT {
            let waiter_state = Arc::clone(&state);
            let waiter_start = Arc::clone(&start_round);
            let waiter_end = Arc::clone(&end_round);
            let waiter_done = done_sender.clone();
            waiters.push(spawn_bounded(move || {
                for round in 0..ROUNDS {
                    waiter_start.wait();
                    waiter_state.wait_for(&[State::Ready]);
                    waiter_done.send(round).expect("stress coordinator dropped");
                    waiter_end.wait();
                }
            }));
        }
        drop(done_sender);

        let setter_state = Arc::clone(&state);
        let setter_start = Arc::clone(&start_round);
        let setter_end = Arc::clone(&end_round);
        let (setter, setter_finished) = spawn_bounded(move || {
            for round in 0..ROUNDS {
                setter_state.set(State::Booting);
                setter_start.wait();
                setter_state.set(State::Ready);

                for _ in 0..WAITER_COUNT {
                    assert_eq!(
                        done_receiver.recv_timeout(TEST_TIMEOUT),
                        Ok(round),
                        "waiter did not observe round {round}"
                    );
                }
                setter_end.wait();
            }
        });

        let deadline = Instant::now() + STRESS_TIMEOUT;
        join_by_deadline(setter, setter_finished, deadline);
        for (waiter, finished) in waiters {
            join_by_deadline(waiter, finished, deadline);
        }
        assert!(state.lock().subscribers.is_empty());
    }

    struct TestWatchdog {
        cancel: Sender<()>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestWatchdog {
        fn start() -> Self {
            let (cancel, receiver) = mpsc::channel();
            let handle = thread::spawn(move || {
                if receiver.recv_timeout(WATCHDOG_TIMEOUT) == Err(RecvTimeoutError::Timeout) {
                    eprintln!(
                        "state test exceeded {WATCHDOG_TIMEOUT:?}; aborting the test process"
                    );
                    std::process::abort();
                }
            });
            Self {
                cancel,
                handle: Some(handle),
            }
        }
    }

    impl Drop for TestWatchdog {
        fn drop(&mut self) {
            let _ = self.cancel.send(());
            self.handle
                .take()
                .expect("watchdog thread handle is present")
                .join()
                .expect("watchdog thread did not panic");
        }
    }

    fn wait_for_subscriber_count(state: &ThreadState, expected: usize, deadline: Instant) {
        loop {
            let actual = state.lock().subscribers.len();
            if actual == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} subscribers, found {actual}"
            );
            thread::yield_now();
        }
    }

    fn spawn_bounded<T, F>(task: F) -> (JoinHandle<T>, Receiver<()>)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (finished_sender, finished_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(task));
            let _ = finished_sender.send(());
            match outcome {
                Ok(value) => value,
                Err(payload) => panic::resume_unwind(payload),
            }
        });
        (handle, finished_receiver)
    }

    fn join_by_deadline<T>(handle: JoinHandle<T>, finished: Receiver<()>, deadline: Instant) -> T {
        let remaining = deadline.saturating_duration_since(Instant::now());
        finished
            .recv_timeout(remaining)
            .expect("worker did not finish before the test deadline");
        unwrap_join(handle.join())
    }

    fn unwrap_join<T>(result: thread::Result<T>) -> T {
        match result {
            Ok(value) => value,
            Err(payload) => panic::resume_unwind(payload),
        }
    }
}
