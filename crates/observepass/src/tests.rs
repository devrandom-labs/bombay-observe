//! Real-thread tests for the blocking wait path and cancellation.
//!
//! The frozen integration tests cover `try_get` semantics; the park/unpark
//! wait mechanism needs its own stress coverage, which loom cannot schedule
//! at the OS level.

use std::sync::{Arc, Barrier};

use crate::ObservationSpace;

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
