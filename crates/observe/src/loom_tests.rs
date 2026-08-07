//! Loom models over the real implementation (map side, generation safety,
//! completion publication, and the blocking wait path).
//!
//! Runs with `RUSTFLAGS="--cfg loom" cargo test -p observe --lib
//! --release`; the frozen gate runs these with `LOOM_MAX_PREEMPTIONS=3`.

use loom::thread;
use loom::thread::yield_now;

use crate::ObservationSpace;

/// Registration racing completion must not lose a published outcome: an
/// observer that captured the slot before completion still reads it after.
#[test]
fn observe_racing_complete_never_loses_outcome() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let observer = thread::spawn(move || {
            loop {
                if let Some(outcome) = observation.try_get() {
                    return outcome;
                }
                yield_now();
            }
        });
        subject.complete(42_u64);
        assert_eq!(observer.join().unwrap(), 42_u64);
    });
}

/// A late retire (drop of an older subject handle) must not remove a
/// replacement generation at the same key.
#[test]
fn stale_retire_cannot_remove_replacement() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let subject_one = space.subject(7_u64).unwrap();
        let retire = thread::spawn(move || drop(subject_one));
        // Registration may fail until the old generation is retired; retry
        // until the replacement exists.
        let mut replacement = None;
        while replacement.is_none() {
            if let Ok(subject) = space.subject(7_u64) {
                replacement = Some(subject);
            } else {
                yield_now();
            }
        }
        retire.join().unwrap();
        // The replacement generation is still retained and observable.
        assert!(space.observe(&7_u64).is_ok());
        assert!(space.subject(7_u64).is_err());
    });
}

/// Concurrent registration at one key: exactly one subject wins.
#[test]
fn concurrent_subject_one_winner() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        // The winner's `Subject` stays alive inside the join result, so the
        // entry remains retained until both registrations have happened.
        let first = thread::spawn({
            let space = space.clone();
            move || space.subject(7_u64)
        });
        let second = thread::spawn({
            let space = space.clone();
            move || space.subject(7_u64)
        });
        // Both results (holding the winner's `Subject`) stay alive until
        // after both joins, so the retained entry is visible to the second
        // registration in every schedule.
        let first_result = first.join().unwrap();
        let second_result = second.join().unwrap();
        assert_eq!(
            u8::from(first_result.is_ok()) + u8::from(second_result.is_ok()),
            1
        );
    });
}

/// A blocking observer is woken by completion with the published outcome,
/// including the registration-races-completion interleavings.
#[test]
fn waiter_wakes_with_outcome() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let observer = thread::spawn(move || observation.wait());
        yield_now();
        subject.complete(9_u64);
        assert_eq!(observer.join().unwrap(), 9_u64);
    });
}

/// Completion before registration: a late observer reads the retained outcome.
#[test]
fn late_observer_reads_retained_outcome() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        subject.complete(5_u64);
        let observation = space.observe(&7_u64).unwrap();
        assert_eq!(observation.try_get(), Some(5_u64));
    });
}

/// A recycled slot is fully reset: the stale COMPLETED bit of the previous
/// generation must not leak into the replacement.
#[test]
fn pooled_slot_reuse_isolates_generations() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        {
            let mut subject = space.subject(7_u64).unwrap();
            subject.complete(1_u64);
        }
        // No observers ever existed, so the slot was pooled and is reused.
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        assert_eq!(observation.try_get(), None);
        subject.complete(2_u64);
        assert_eq!(observation.try_get(), Some(2_u64));
    });
}

/// Promotion from the inline vector to the hash map preserves observe and
/// retire semantics, including reuse after retirement.
#[test]
fn small_map_promotion_preserves_semantics() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subjects = Vec::new();
        for key in 0..5_u64 {
            subjects.push(space.subject(key).expect("fresh key"));
        }
        for key in 0..5_u64 {
            assert!(space.observe(&key).is_ok());
        }
        subjects.clear();
        assert!(space.subject(0_u64).is_ok());
    });
}

/// Two concurrent waiters: the drain wakes both, each with the outcome.
#[test]
fn multiple_waiters_all_wake() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let first = space.observe(&7_u64).unwrap();
        let second = space.observe(&7_u64).unwrap();
        let first_waiter = thread::spawn(move || first.wait());
        let second_waiter = thread::spawn(move || second.wait());
        yield_now();
        subject.complete(9_u64);
        assert_eq!(first_waiter.join().unwrap(), 9_u64);
        assert_eq!(second_waiter.join().unwrap(), 9_u64);
    });
}

/// A waiter that registers after completion returns without parking.
#[test]
fn waiter_after_completion_returns_immediately() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        subject.complete(4_u64);
        let observation = space.observe(&7_u64).unwrap();
        assert_eq!(observation.wait(), 4_u64);
    });
}

/// `into_outcome` racing completion is never torn: it either sees the
/// pending state or the fully published outcome.
#[test]
fn into_outcome_racing_complete() {
    loom::model(|| {
        #[derive(Debug, PartialEq, Eq)]
        struct Handle(u64);
        let space: ObservationSpace<u64, Handle> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let observer = thread::spawn(move || observation.into_outcome());
        subject.complete(Handle(9));
        drop(subject);
        let result = observer.join().unwrap();
        assert!(result.is_none() || result == Some(Handle(9)));
    });
}

/// The wait_timeout registration protocol matches wait() under the model
/// (the clock is not modeled; the completion path is what matters).
#[test]
fn wait_timeout_wakes_with_outcome() {
    loom::model(|| {
        let space: ObservationSpace<u64, u64> = ObservationSpace::new();
        let mut subject = space.subject(7_u64).unwrap();
        let observation = space.observe(&7_u64).unwrap();
        let observer =
            thread::spawn(move || observation.wait_timeout(std::time::Duration::from_secs(1)));
        yield_now();
        subject.complete(9_u64);
        assert_eq!(observer.join().unwrap(), Some(9_u64));
    });
}
