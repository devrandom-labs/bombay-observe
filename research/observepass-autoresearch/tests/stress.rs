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

use observepass::{ObservationSpace, Subject};
use observepass_autoresearch::probe::{CountWake, DropProbe, ThreadWake};

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

/// Raw-API waker registration racing completion: the waiter thread calls
/// `register_waker` directly (no future), then blocks on `park`.
/// `register_waker` returning `false` is a promise: "registered, you WILL
/// be woken". The completion drain must fire the waker whether the
/// registration won or lost the race. A lost wake strands the parked
/// waiter; the `recv_timeout` watchdog turns that into a hard failure
/// (and `Ok(None)` would catch a wake fired before the publication was
/// observable, an ordering violation).
#[test]
fn stress_register_waker_racing_completion_no_lost_wake() {
    const ROUNDS: u64 = 400;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(), // previous round's subject not yet dropped
            }
        };
        let observation = space.observe(&key).expect("live generation");
        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let (waker, _) = ThreadWake::waker();
                if observation.register_waker(&waker) {
                    // Already published: nothing registered, read directly.
                    tx.send(observation.try_get()).expect("send failed");
                    return;
                }
                // Registered: block until the completion drain unparks us.
                // A lost wakeup strands this park forever -> watchdog.
                std::thread::park();
                tx.send(observation.try_get()).expect("send failed");
            })
        };
        barrier.wait();
        // Bias one round in four toward the registered-then-completed
        // ordering (registration wins the race); the rest race freely.
        if round % 4 == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        subject.complete(round);
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(outcome)) => assert_eq!(outcome, round, "wrong outcome (round {round})"),
            Ok(None) => panic!(
                "register_waker returned false but the outcome was not readable after the wake (round {round})"
            ),
            Err(_) => panic!(
                "register_waker returned false but no wake arrived: waiter stranded (round {round})"
            ),
        }
        waiter.join().expect("waiter panicked");
    }
}

/// SubjectExists storm under pool churn: a keeper holds key 2 permanently
/// while four threads hammer `subject(2)` — every attempt pops a pooled
/// slot and must restore it on `SubjectExists` (a leaked pop would shrink
/// the pool). Concurrently a churner cycles 300 generations on key 1 (past
/// the 128-slot pool cap), each completed with a uniquely counted probe.
/// Every failed registration must return `SubjectExists` (never a win),
/// and every completed outcome must be destroyed exactly once.
#[test]
fn stress_subject_exists_storm_pool_churn() {
    const STORMERS: usize = 4;
    const CHURN: u64 = 300;

    let space = Arc::new(ObservationSpace::<u8, DropProbe>::new());
    let counter = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // The keeper holds key 2 for the whole test; stormers can never win it.
    let _keeper = space.subject(2).expect("keeper registers");

    let stormers: Vec<_> = (0..STORMERS)
        .map(|id| {
            let space = Arc::clone(&space);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut conflicts = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    match space.subject(2) {
                        Ok(_won) => panic!("stormer {id} won a key the keeper holds"),
                        Err(_) => conflicts += 1,
                    }
                }
                conflicts
            })
        })
        .collect();

    for round in 0..CHURN {
        let mut subject = loop {
            match space.subject(1) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        subject.complete(DropProbe::with_counter(round, &counter));
        // drop(subject): retire; the pooled slot is recycled past the cap.
    }

    stop.store(true, Ordering::SeqCst);
    let conflicts: usize = stormers
        .into_iter()
        .map(|s| s.join().expect("stormer panicked"))
        .sum();
    assert!(conflicts > 0, "stormers never contended for the key");
    drop(_keeper);
    drop(space);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        CHURN as usize,
        "completed outcomes must be destroyed exactly once under the storm"
    );
}

/// Mixed drain on ONE generation: two blocking thread waiters, two
/// `wait_timeout` waiters, and two raw waker registrations, all racing one
/// completion. The drain must resolve every waiter exactly once with the
/// exact outcome — the native counterpart of the loom mixed-drain model
/// that proved infeasible under the scheduler (recorded in Batch 16).
#[test]
fn stress_mixed_waiter_drain_all_resolved() {
    const ROUNDS: u64 = 200;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let barrier = Arc::new(Barrier::new(7)); // 6 waiters + publisher

        let thread_waiters: Vec<_> = (0..2)
            .map(|_| {
                let observation = space.observe(&key).expect("live generation");
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    observation.wait()
                })
            })
            .collect();

        let timeout_waiters: Vec<_> = (0..2)
            .map(|_| {
                let observation = space.observe(&key).expect("live generation");
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    observation.wait_timeout(Duration::from_secs(5))
                })
            })
            .collect();

        let waker_waiters: Vec<_> = (0..2)
            .map(|_| {
                let observation = space.observe(&key).expect("live generation");
                let barrier = Arc::clone(&barrier);
                let (waker, probe) = CountWake::waker();
                thread::spawn(move || {
                    barrier.wait();
                    // Either the registration won (false: will be woken) or
                    // the completion won (true: already published) — both
                    // legal; the outcome must be readable either way.
                    if !observation.register_waker(&waker) {
                        // Spin until the waker fires, then read. A lost
                        // wake strands this loop; the deadline fails it.
                        let deadline = std::time::Instant::now() + Duration::from_secs(10);
                        while probe.count() == 0 {
                            if std::time::Instant::now() > deadline {
                                panic!("registered waker never fired");
                            }
                            thread::yield_now();
                        }
                    }
                    observation
                        .try_get()
                        .expect("outcome must be readable after the wake")
                })
            })
            .collect();

        barrier.wait();
        subject.complete(round);

        for waiter in thread_waiters {
            assert_eq!(waiter.join().expect("thread waiter panicked"), round);
        }
        for waiter in timeout_waiters {
            assert_eq!(
                waiter.join().expect("timeout waiter panicked"),
                Some(round),
                "timeout waiter must resolve to the exact outcome"
            );
        }
        for waiter in waker_waiters {
            assert_eq!(
                waiter.join().expect("waker waiter panicked"),
                round,
                "waker waiter must resolve to the exact outcome"
            );
        }
    }
}

/// Same-key contention: a generation is captured and retired, then two
/// threads race `subject(key)` for the now-vacant key from a barrier.
/// Exactly one must win; the loser gets `SubjectExists`. The pre-race
/// observation must resolve to the pre-race generation's value (never the
/// winner's), and the winner's own generation must be observable with its
/// tagged value. 500 rounds, value tags identify the winning publisher.
#[test]
fn stress_same_key_contention_exactly_one_winner() {
    const ROUNDS: u64 = 500;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        // Pre-race generation: captured, completed, retired.
        let mut subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let observer = space.observe(&key).expect("live generation");
        subject.complete(round);
        drop(subject); // retire: the key is vacant for the contenders

        // Two-phase barrier: `start` releases both contenders together;
        // `release` is crossed only AFTER both have attempted subject().
        // A one-shot barrier is not enough — the first contender could
        // complete, retire, and free the key before the second thread is
        // even scheduled, which is not a race at all.
        let start = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let contender = |tag: u64, start: &Arc<Barrier>, release: &Arc<Barrier>| {
            let space = Arc::clone(&space);
            let start = Arc::clone(start);
            let release = Arc::clone(release);
            thread::spawn(move || {
                start.wait();
                // Register, binding the subject OUTSIDE the match: it must
                // stay alive across `release` so both attempts overlap.
                // (A match-arm local would retire at the arm's end, freeing
                // the key before the other contender even tries.)
                let won: Option<Subject<u8, u64>> = space.subject(key).ok();
                release.wait();
                match won {
                    Some(mut subject) => {
                        subject.complete((tag << 56) | round);
                        // The published generation must be observable while
                        // still retained (before this subject retires).
                        assert_eq!(
                            space.observe(&key).expect("winner retained").try_get(),
                            Some((tag << 56) | round),
                            "winner's published generation not observable"
                        );
                        Some(tag)
                    }
                    None => None,
                }
            })
        };
        let a = contender(1, &start, &release);
        let b = contender(2, &start, &release);
        start.wait();
        release.wait();
        let winner_a = a.join().expect("contender A panicked");
        let winner_b = b.join().expect("contender B panicked");
        let winners = usize::from(winner_a.is_some()) + usize::from(winner_b.is_some());
        assert_eq!(winners, 1, "round {round}: exactly one subject must win the key");
        let _winner = winner_a.or(winner_b).expect("a winner exists");

        // The pre-race observation resolves to the pre-race value, never
        // the winner's.
        assert_eq!(
            observer.wait(),
            round,
            "round {round}: pre-race observer resolved to the winner's generation"
        );
        // The winner's subject dropped when its contender thread ended:
        // the key is vacant for the next round.
    }
}

/// Pinned retired-pending generations never fabricate: observations
/// captured on generations that are retired WITHOUT completing must time
/// out forever (never resolve to a value), even while other generations on
/// the same key complete concurrently. The threaded twin of pool.rs's
/// `uncompleted_generation_recycles_without_fabricating`.
#[test]
fn stress_pinned_pending_timeout_never_fabricates() {
    const ROUNDS: u64 = 200;

    let space = Arc::new(ObservationSpace::<u8, u64>::new());
    for round in 0..ROUNDS {
        let key = (round % 2) as u8;
        // Capture a generation, retire it WITHOUT completing. Two handles
        // to the same slot: one stays on this thread, one goes to the
        // waiter.
        let subject = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let pinned = space.observe(&key).expect("live generation");
        let pinned_waiter = space.observe(&key).expect("live generation");
        drop(subject); // retire pending; both handles hold the old slot

        // Churn a NEW generation on the same key to completion while the
        // pinned observation waits: it must never see the new value.
        let mut churn = loop {
            match space.subject(key) {
                Ok(subject) => break subject,
                Err(_) => thread::yield_now(),
            }
        };
        let barrier = Arc::new(Barrier::new(2));
        let waiter = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                // The old slot never completes; this must always time out.
                pinned_waiter.wait_timeout(Duration::from_millis(20))
            })
        };
        barrier.wait();
        churn.complete(round);
        assert_eq!(
            waiter.join().expect("waiter panicked"),
            None,
            "a retired-pending generation must never fabricate an outcome (round {round})"
        );
        assert_eq!(
            pinned.try_get(),
            None,
            "the pinned observation must stay pending (round {round})"
        );
    }
}
