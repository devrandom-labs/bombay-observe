//! Real-thread tests for the blocking wait path and cancellation.
//!
//! The frozen integration tests cover `try_get` semantics; the park/unpark
//! wait mechanism needs its own stress coverage, which loom cannot schedule
//! at the OS level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::task::Wake;
use std::time::{Duration, Instant};

use crate::ObservationSpace;

/// A waker that raises a flag when woken.
struct FlagWake(AtomicBool);

impl Wake for FlagWake {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A waiter registered before completion receives the published outcome.
#[test]
fn waiter_receives_published_outcome() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    let waiter = std::thread::spawn(move || observation.wait());
    std::thread::yield_now();
    subject.complete(9_u64);
    assert_eq!(waiter.join().unwrap(), 9_u64);
}

/// Completion before wait returns immediately with the retained outcome.
#[test]
fn completion_before_wait_returns_immediately() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    subject.complete(3_u64);
    let observation = space.observe(&7_u64).unwrap();
    assert_eq!(observation.wait(), 3_u64);
}

/// Fanout waiters: every registered waiter is woken exactly once.
#[test]
fn multiple_waiters_all_receive_the_outcome() {
    const WAITERS: usize = 8;
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let barrier = Arc::new(Barrier::new(WAITERS + 1));
    let mut handles = Vec::with_capacity(WAITERS);
    for _ in 0..WAITERS {
        let observation = space.observe(&7_u64).unwrap();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            observation.wait()
        }));
    }
    barrier.wait();
    subject.complete(9_u64);
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 9_u64);
    }
}

/// Repeated wait-versus-complete races never lose the outcome.
#[test]
fn wait_racing_complete_never_loses_outcome() {
    for _ in 0..100 {
        let space = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let waiter = std::thread::spawn(move || observation.wait());
        subject.complete(9_u64);
        assert_eq!(waiter.join().unwrap(), 9_u64);
    }
}

/// Dropping an observer (cancellation) cannot obstruct completion.
#[test]
fn cancelled_observer_does_not_block_completion() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let cancelled = space.observe(&7_u64).unwrap();
    drop(cancelled);
    let observer = space.observe(&7_u64).unwrap();
    subject.complete(5_u64);
    assert_eq!(observer.try_get(), Some(5_u64));
}

/// Move-only outcomes: `into_outcome` moves the value out when this handle
/// is the last reference to the slot.
#[test]
fn into_outcome_moves_non_clone_outcome() {
    #[derive(Debug, PartialEq, Eq)]
    struct Handle(u64);
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    subject.complete(Handle(5));
    drop(subject);
    assert_eq!(observation.into_outcome(), Some(Handle(5)));
}

/// `into_outcome` returns `None` while the outcome is pending or the slot is
/// still shared, and succeeds for the last reference after retirement.
#[test]
fn into_outcome_none_while_shared_or_pending() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let pending = space.observe(&7_u64).unwrap();
    assert_eq!(pending.into_outcome(), None);
    subject.complete(9_u64);
    let shared = space.observe(&7_u64).unwrap();
    assert_eq!(shared.into_outcome(), None);
    let last = space.observe(&7_u64).unwrap();
    drop(subject);
    assert_eq!(last.into_outcome(), Some(9_u64));
}

/// A registered waker fires when the outcome is published.
#[test]
fn register_waker_wakes_on_completion() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    let flag = Arc::new(FlagWake(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&flag));
    assert!(!observation.register_waker(&waker));
    subject.complete(9_u64);
    assert!(flag.0.load(Ordering::Relaxed));
    assert_eq!(observation.try_get(), Some(9_u64));
}

/// Registration after publication reports the outcome as already available.
#[test]
fn register_waker_after_completion_returns_true() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    subject.complete(3_u64);
    let observation = space.observe(&7_u64).unwrap();
    assert!(observation.register_waker(std::task::Waker::noop()));
}

/// Registration racing completion never loses the outcome: either the waker
/// fires (the drain saw the registration) or the registration reported the
/// outcome as already published; the outcome is readable either way.
#[test]
fn register_waker_racing_complete_never_loses_outcome() {
    for _ in 0..100 {
        let space = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let flag = Arc::new(FlagWake(AtomicBool::new(false)));
        let waker = std::task::Waker::from(Arc::clone(&flag));
        let completer = std::thread::spawn(move || subject.complete(9_u64));
        let registered = observation.register_waker(&waker);
        completer.join().unwrap();
        assert_eq!(observation.try_get(), Some(9_u64));
        if !registered {
            assert!(flag.0.load(Ordering::Relaxed));
        }
    }
}

/// A pending subject times out without returning a fabricated outcome.
#[test]
fn wait_timeout_returns_none_when_pending() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    let started = Instant::now();
    assert_eq!(observation.wait_timeout(Duration::from_millis(10)), None);
    assert!(started.elapsed() >= Duration::from_millis(9));
    subject.complete(9_u64);
    assert_eq!(observation.try_get(), Some(9_u64));
}

/// A completion during the wait is delivered before the deadline.
#[test]
fn wait_timeout_returns_outcome_when_completed() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    let completer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        subject.complete(9_u64);
    });
    assert_eq!(
        observation.wait_timeout(Duration::from_secs(5)),
        Some(9_u64)
    );
    completer.join().unwrap();
}

/// Registering the same task's waker twice keeps a single entry (repeated
/// polling never accumulates duplicates).
///
/// Uses a waker with a single static vtable: the std `Wake`-derived vtable
/// is a const-promoted temporary whose address differs between code sites
/// under Miri, which would make `will_wake` spuriously false.
#[test]
fn register_waker_is_idempotent_per_task() {
    use crate::{Waiter, lock};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    struct RawFlagWaker(AtomicBool);

    unsafe fn raw_clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &RAW_VTABLE)
    }

    unsafe fn raw_wake(data: *const ()) {
        // SAFETY: the data is always the RawFlagWaker created by this test,
        // which outlives every wake call.
        let flag = unsafe { &*(data.cast::<RawFlagWaker>()) };
        flag.0.store(true, Ordering::Relaxed);
    }

    unsafe fn raw_wake_by_ref(data: *const ()) {
        // SAFETY: same contract as `raw_wake`.
        unsafe { raw_wake(data) };
    }

    unsafe fn raw_drop(_data: *const ()) {}

    static RAW_VTABLE: RawWakerVTable =
        RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);

    let flag = Arc::new(RawFlagWaker(AtomicBool::new(false)));
    // SAFETY: the pointer is valid for the test's lifetime (the Arc is held
    // here); clone/drop do not dereference it.
    let waker = unsafe {
        Waker::from_raw(RawWaker::new(
            std::ptr::from_ref(Arc::as_ref(&flag)).cast(),
            &RAW_VTABLE,
        ))
    };
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    assert!(!observation.register_waker(&waker));
    assert!(!observation.register_waker(&waker));
    let waiters = lock(&observation.slot.waiters);
    assert_eq!(waiters.len(), 1);
    assert!(matches!(&waiters[0], Waiter::Waker(w) if w.will_wake(&waker)));
    drop(waiters);
    subject.complete(9_u64);
    assert!(flag.0.load(Ordering::Relaxed));
    assert_eq!(observation.try_get(), Some(9_u64));
}

/// The observation's `IntoFuture` resolves to the outcome on completion.
#[test]
fn observation_future_resolves_on_completion() {
    use std::future::IntoFuture;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let mut future = space.observe(&7_u64).unwrap().into_future();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    subject.complete(9_u64);
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(9_u64)
    ));
}

/// Dropping the future deregisters its waker: completion after a cancelled
/// future does not fire it, and the slot's outcome stays readable.
#[test]
fn dropping_observation_future_deregisters_waker() {
    use std::future::IntoFuture;
    use std::pin::Pin;
    use std::task::Context;

    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let observation = space.observe(&7_u64).unwrap();
    let flag = Arc::new(FlagWake(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&flag));
    let mut cx = Context::from_waker(&waker);
    let mut future = observation.into_future();
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    drop(future);
    subject.complete(9_u64);
    assert!(!flag.0.load(Ordering::Relaxed));
    assert_eq!(space.observe(&7_u64).unwrap().try_get(), Some(9_u64));
}
