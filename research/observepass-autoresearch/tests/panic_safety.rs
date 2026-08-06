//! Panic-safety of the completion drain: a user waker may panic (the
//! `Wake` contract does not forbid it). One panicking waker must not
//! strand the other waiters of the same generation — a parked thread
//! waiter must still be unparked, and later wakers must still fire.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Wake;
use std::time::Duration;

use observepass::ObservationSpace;
use observepass_autoresearch::probe::CountWake;

/// A waker that panics when woken.
struct PanicWake;

impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("user waker panicked");
    }
}

/// FINDING-002 — minimal reproducer. See RESEARCH-REPORT.md.
///
/// Registration order: panicking waker FIRST, then a well-behaved waker,
/// then a parked thread waiter. Completion drains in registration order;
/// the correct behavior is that every waiter is woken despite the panic.
/// Today the panic aborts the drain: the later waker never fires and the
/// parked thread is never unparked.
#[test]
#[ignore = "FINDING-002: panicking waker aborts the completion drain; later wakers skipped (0 fires) and parked thread waiters stranded"]
fn panicking_waker_must_not_strand_other_waiters() {
    let space = ObservationSpace::<u8, u64>::new();
    let mut subject = space.subject(1).expect("first registration succeeds");

    let obs1 = space.observe(&1).expect("subject retained");
    assert!(!obs1.register_waker(&std::task::Waker::from(Arc::new(PanicWake))));

    let obs2 = space.observe(&1).expect("subject retained");
    let (good_waker, good_probe) = CountWake::waker();
    assert!(!obs2.register_waker(&good_waker));

    let obs3 = space.observe(&1).expect("subject retained");
    let waiter = std::thread::spawn(move || obs3.wait());
    let handle = waiter.thread().clone();
    // Ensure the waiter is registered AND parked before completion, so the
    // stranding is deterministic (not masked by the post-registration
    // recheck seeing COMPLETED).
    std::thread::sleep(Duration::from_millis(50));

    // The completing thread itself must tolerate the panic path; catch it
    // here so the test can assert on the other waiters.
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subject.complete(42);
    }));
    drop(panic); // whether complete propagates the panic is not the issue

    std::thread::sleep(Duration::from_millis(100));
    let stranded = !waiter.is_finished();
    handle.unpark(); // cleanup: break any park so the thread can finish
    assert_eq!(waiter.join().expect("waiter thread panicked"), 42);
    assert!(
        !stranded,
        "a parked waiter was stranded by an earlier waker's panic"
    );
    assert_eq!(
        good_probe.count(),
        1,
        "a later waker was skipped by an earlier waker's panic (count {})",
        good_probe.count()
    );
}

/// The inverse order: well-behaved waiters registered BEFORE the panicking
/// one must be unaffected (they are drained first).
#[test]
fn waiters_before_the_panicking_one_are_woken() {
    let space = ObservationSpace::<u8, u64>::new();
    let mut subject = space.subject(2).expect("first registration succeeds");

    let obs1 = space.observe(&2).expect("subject retained");
    let (good_waker, good_probe) = CountWake::waker();
    assert!(!obs1.register_waker(&good_waker));

    let obs2 = space.observe(&2).expect("subject retained");
    assert!(!obs2.register_waker(&std::task::Waker::from(Arc::new(PanicWake))));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subject.complete(7);
    }));
    drop(panic);

    assert_eq!(
        good_probe.count(),
        1,
        "waiter registered before the panicking one must still fire"
    );
}

/// After a panicking drain, the outcome is still published and readable,
/// and the slot can be retired and recycled without further fallout
/// (drop counts stay exactly-once).
#[test]
fn state_after_panicking_drain_stays_consistent() {
    use observepass_autoresearch::probe::DropProbe;

    let space = ObservationSpace::<u8, DropProbe>::new();
    let (probe, counter) = DropProbe::new(1);
    let mut subject = space.subject(3).expect("first registration succeeds");

    let obs = space.observe(&3).expect("subject retained");
    assert!(!obs.register_waker(&std::task::Waker::from(Arc::new(PanicWake))));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subject.complete(probe);
    }));
    drop(panic);

    let (completion_waker, _) = observepass_autoresearch::probe::CountWake::waker();
    assert!(
        obs.register_waker(&completion_waker),
        "outcome must be published even if the drain panicked"
    );
    drop(obs);
    drop(subject);
    drop(space);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "outcome dropped exactly once after a panicking drain"
    );
}
