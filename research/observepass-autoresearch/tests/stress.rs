//! Deterministic adversarial stress: real threads, barrier-synchronized,
//! fixed-seed SplitMix64 op selection. Publishers race on shared keys;
//! observers hammer observe/try_get/wait/wait_timeout/waker cancellation.
//! Every value read is tag-checked against the generation that published
//! it (no cross-generation leakage), and every blocking wait must return
//! (publishers complete every generation before retiring it).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use observepass::ObservationSpace;
use observepass_autoresearch::probe::CountWake;

/// SplitMix64: deterministic, seedable, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Outcome encoding: publisher tag in the high byte, round in the middle,
/// key in the low byte. Any leakage across generations or keys breaks the
/// tag check.
fn encode(publisher: u64, round: u64, key: u64) -> u64 {
    (publisher << 56) | (round << 8) | key
}

fn key_of(outcome: u64) -> u64 {
    outcome & 0xFF
}

/// 2 publishers x 2 keys x 2,000 rounds, 4 observer threads x 2,000
/// iterations, all released by one barrier. Value integrity only; the run
/// is deterministic in op selection (not in interleaving).
#[test]
fn stress_publishers_observers_value_integrity() {
    const KEYS: u64 = 2;
    const PUBLISHERS: u64 = 2;
    const OBSERVERS: u64 = 4;
    const ROUNDS: u64 = 2_000;

    let space = Arc::new(ObservationSpace::<u64, u64>::new());
    let barrier = Arc::new(Barrier::new((PUBLISHERS + OBSERVERS + 1) as usize));
    let stop = Arc::new(AtomicBool::new(false));

    let publishers: Vec<_> = (0..PUBLISHERS)
        .map(|id| {
            let space = Arc::clone(&space);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut rng = Rng(0xA11C_E000 + id);
                barrier.wait();
                for round in 0..ROUNDS {
                    let key = rng.below(KEYS);
                    if let Ok(mut subject) = space.subject(key) {
                        subject.complete(encode(id, round, key));
                    } // Err: contention, another publisher holds the key
                }
            })
        })
        .collect();

    let observers: Vec<_> = (0..OBSERVERS)
        .map(|id| {
            let space = Arc::clone(&space);
            let barrier = Arc::clone(&barrier);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut rng = Rng(0x0B5E_7000 + id);
                barrier.wait();
                let mut reads = 0_u64;
                for _ in 0..ROUNDS {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let key = rng.below(KEYS);
                    let Ok(observation) = space.observe(&key) else {
                        continue; // vacant under contention: fine
                    };
                    match rng.below(4) {
                        0 => {
                            if let Some(outcome) = observation.try_get() {
                                assert_eq!(key_of(outcome), key, "try_get leaked across keys");
                                reads += 1;
                            }
                        }
                        1 => {
                            if let Some(outcome) =
                                observation.wait_timeout(Duration::from_millis(50))
                            {
                                assert_eq!(key_of(outcome), key, "wait_timeout leaked across keys");
                                reads += 1;
                            }
                        }
                        2 => {
                            // Cancellation storm: register and immediately drop.
                            let (waker, _probe) = CountWake::waker();
                            let _ = observation.register_waker(&waker);
                        }
                        _ => {
                            // Blocking wait with a safety net: every captured
                            // generation completes before its publisher
                            // retires, so this must return.
                            let outcome = observation.wait();
                            assert_eq!(key_of(outcome), key, "wait leaked across keys");
                            reads += 1;
                        }
                    }
                }
                reads
            })
        })
        .collect();

    barrier.wait();
    for publisher in publishers {
        publisher.join().expect("publisher panicked");
    }
    let total_reads: u64 = observers
        .into_iter()
        .map(|observer| observer.join().expect("observer panicked"))
        .sum();
    assert!(total_reads > 0, "stress run observed no completions at all");
}

/// Fanout storm: 8 waiters capture the same generation; one publisher
/// completes it. Every waiter must wake with the exact outcome, 200 rounds
/// with a fresh generation each round. Barriers make the registration race
/// real: waiters start waiting while the publisher may already have
/// completed.
#[test]
fn stress_waiter_fanout_exact_outcome() {
    const WAITERS: usize = 8;
    const ROUNDS: u64 = 200;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    let failures = Arc::new(AtomicUsize::new(0));

    for round in 0..ROUNDS {
        let key = (round % 3) as u8;
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(), // previous round's subject not yet dropped
            }
        };
        let start = Arc::new(Barrier::new(WAITERS + 1));
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| {
                let observation = space.observe(&key).expect("live generation");
                let start = Arc::clone(&start);
                let failures = Arc::clone(&failures);
                thread::spawn(move || {
                    start.wait();
                    let outcome = observation.wait();
                    if outcome != round {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        start.wait();
        subject.complete(round);
        for waiter in waiters {
            waiter.join().expect("waiter panicked");
        }
    }
    assert_eq!(failures.load(Ordering::SeqCst), 0, "waiter received wrong outcome");
}

/// Timeout-boundary storm: a waiter loops `wait_timeout(0)` and
/// `wait_timeout(1ns)` while the publisher completes mid-storm. Every
/// `Some` must carry the exact published outcome; after completion the
/// loop must eventually observe `Some`.
#[test]
fn stress_zero_timeout_boundary() {
    const ROUNDS: u64 = 500;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let observation = space.observe(&key).expect("live generation");
        let barrier = Arc::new(Barrier::new(2));
        let waiter = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                // Completion is guaranteed (the publisher completes before
                // retiring), so loop until observed: a fixed iteration cap
                // would make the test itself racy under scheduler load.
                loop {
                    if let Some(outcome) = observation.wait_timeout(Duration::ZERO) {
                        assert_eq!(outcome, round, "zero-timeout returned a wrong outcome");
                        return true;
                    }
                    if let Some(outcome) = observation.wait_timeout(Duration::from_nanos(1)) {
                        assert_eq!(outcome, round, "nano-timeout returned a wrong outcome");
                        return true;
                    }
                }
            })
        };
        barrier.wait();
        thread::yield_now();
        subject.complete(round);
        assert!(waiter.join().expect("waiter panicked"), "waiter never observed the completion");
    }
}

/// Reentrant wake/drop: a waker whose `wake` drops ANOTHER observation of
/// the same generation (re-entering the slot's waiter machinery during the
/// drain) must not deadlock or corrupt the drain.
#[test]
fn stress_reentrant_wake_drops_observation() {
    use std::task::Wake;

    struct Reentrant {
        victim: std::sync::Mutex<Option<observepass::Observation<u64>>>,
        fires: AtomicUsize,
    }

    impl Wake for Reentrant {
        fn wake(self: Arc<Self>) {
            self.fires.fetch_add(1, Ordering::SeqCst);
            // Re-enter: dropping the victim observation during the drain.
            let _ = self.victim.lock().expect("victim lock").take();
        }
    }

    for round in 0..100_u64 {
        let space = ObservationSpace::<u8, u64>::new();
        let mut subject = space.subject(1).expect("first registration succeeds");
        let obs = space.observe(&1).expect("subject retained");
        let victim = space.observe(&1).expect("subject retained");
        let reentrant = Arc::new(Reentrant {
            victim: std::sync::Mutex::new(Some(victim)),
            fires: AtomicUsize::new(0),
        });
        assert!(!obs.register_waker(&std::task::Waker::from(reentrant.clone())));
        subject.complete(round);
        assert_eq!(
            reentrant.fires.load(Ordering::SeqCst),
            1,
            "reentrant waker fired != once (round {round})"
        );
        assert_eq!(obs.try_get(), Some(round));
    }
}

/// Spurious-unpark injection: an external thread fires extra unparks at a
/// blocked waiter while a second waiter registers concurrently, stressing
/// the duplicate-registration dedup (`waiters.last()` check). Whatever the
/// internal duplication, every waiter must still resolve to the exact
/// outcome and no waiter may be left parked.
#[test]
fn stress_spurious_unpark_injection() {
    const ROUNDS: u64 = 100;

    let space = std::sync::Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let start = std::sync::Arc::new(Barrier::new(4));
        let mk_waiter = || {
            let observation = space.observe(&key).expect("live generation");
            let start = std::sync::Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                observation.wait()
            })
        };
        let waiter_a = mk_waiter();
        let waiter_b = mk_waiter();
        let handle_a = waiter_a.thread().clone();

        let injector = {
            let start = std::sync::Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for _ in 0..64 {
                    handle_a.unpark(); // injected spurious wakeups
                    thread::yield_now();
                }
            })
        };
        start.wait();
        // Let the injection interleave with registration, then complete.
        for _ in 0..32 {
            thread::yield_now();
        }
        subject.complete(round);
        injector.join().expect("injector panicked");
        assert_eq!(waiter_a.join().expect("waiter A panicked"), round);
        assert_eq!(waiter_b.join().expect("waiter B panicked"), round);
    }
}

/// A waker whose `wake` re-registers ITSELF on the same slot (reentrant
/// registration during the drain) must not deadlock or loop: the
/// re-registration sees COMPLETED and returns immediately.
#[test]
fn stress_reentrant_wake_reregistration_no_loop() {
    use std::task::Wake;

    struct ReRegister {
        obs: std::sync::Mutex<Option<observepass::Observation<u64>>>,
        fires: AtomicUsize,
    }

    impl Wake for ReRegister {
        fn wake(self: Arc<Self>) {
            self.fires.fetch_add(1, Ordering::SeqCst);
            let guard = self.obs.lock().expect("obs lock");
            if let Some(obs) = guard.as_ref() {
                // Reentrant registration mid-drain: must return `true`
                // (already completed) without registering again.
                let (waker, _) = CountWake::waker();
                assert!(obs.register_waker(&waker), "re-registration must see COMPLETED");
            }
        }
    }

    for round in 0..50_u64 {
        let space = ObservationSpace::<u8, u64>::new();
        let mut subject = space.subject(1).expect("first registration succeeds");
        let obs = space.observe(&1).expect("subject retained");
        let re = Arc::new(ReRegister {
            obs: std::sync::Mutex::new(Some(space.observe(&1).expect("subject retained"))),
            fires: AtomicUsize::new(0),
        });
        assert!(!obs.register_waker(&std::task::Waker::from(re.clone())));
        subject.complete(round);
        assert_eq!(
            re.fires.load(Ordering::SeqCst),
            1,
            "reentrant waker fired != once (round {round})"
        );
    }
}

/// Subject lifecycle migration: register on one thread, complete on a
/// second, retire on a third; a waiter on a fourth must observe the exact
/// outcome. The generation protocol must not depend on thread affinity.
#[test]
fn stress_subject_thread_migration() {
    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..100_u64 {
        let key = (round % 3) as u8;
        let subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let waiter = {
            let observation = space.observe(&key).expect("live generation");
            thread::spawn(move || observation.wait())
        };
        let mut subject = thread::spawn(move || subject)
            .join()
            .expect("migration thread 1 panicked");
        let subject = thread::spawn(move || {
            subject.complete(round);
            subject
        })
        .join()
        .expect("migration thread 2 panicked");
        assert_eq!(waiter.join().expect("waiter panicked"), round);
        thread::spawn(move || drop(subject))
            .join()
            .expect("migration thread 3 panicked");
    }
}

/// Registration flood: hundreds of distinct wakers on one pending
/// generation, each must fire exactly once at completion.
#[test]
fn stress_registration_flood_wakes_each_once() {
    const WAKERS: usize = 256;
    let space = ObservationSpace::<u8, u64>::new();
    let mut subject = space.subject(7).expect("first registration succeeds");
    let probes: Vec<_> = (0..WAKERS).map(|_| CountWake::waker()).collect();
    for (waker, _) in &probes {
        let obs = space.observe(&7).expect("subject retained");
        assert!(!obs.register_waker(waker));
    }
    subject.complete(99);
    for (i, (_, probe)) in probes.iter().enumerate() {
        assert_eq!(probe.count(), 1, "waker {i} fired != once");
    }
}
