//! The actorpass adapter pattern, using only the observepass API.
//!
//! One actor generation maps to one `Subject` at a key. The parent registers
//! a `std::task::Waker` (no Tokio) before the child task completes, the task
//! publishes its outcome, and the waker fires. Observations translate into
//! the adapter's own `ChildStopped`/`PeerStopped` vocabulary - observepass
//! itself knows none of it.

use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use observepass::ObservationSpace;

/// Typed outcome of one actor generation (the adapter's vocabulary).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    ChildStopped(u64),
    PeerStopped(u64),
}

/// A waker that raises a flag; a real adapter would wake its executor task
/// instead. The protocol observepass provides is identical either way.
struct Signal(AtomicBool);

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

fn main() {
    let space = ObservationSpace::<u64, Outcome>::new();

    // Spawn a generation: register the subject and capture an observation.
    let mut child = space.subject(7).expect("fresh key");
    let observation = space.observe(&7).expect("subject retained");

    // Register the async waker before the task completes. `register_waker`
    // reports whether the outcome is already published, closing the
    // registration-races-completion race.
    let signal = Arc::new(Signal(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&signal));
    if observation.register_waker(&waker) {
        assert_eq!(observation.try_get(), Some(Outcome::ChildStopped(7)));
        return;
    }

    // The child task publishes its outcome exactly once.
    child.complete(Outcome::ChildStopped(7));

    // Executor loop: the wake signals readiness, then the outcome is read.
    while !signal.0.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    assert_eq!(observation.try_get(), Some(Outcome::ChildStopped(7)));

    // Peer observation after completion reads the retained outcome.
    let peer = space.observe(&7).expect("subject retained");
    assert_eq!(peer.try_get(), Some(Outcome::ChildStopped(7)));

    // Peer fanout: a peer observes its own generation's stop.
    let mut peer_generation = space.subject(8).expect("fresh key");
    let peer_observation = space.observe(&8).expect("subject retained");
    peer_generation.complete(Outcome::PeerStopped(8));
    assert_eq!(peer_observation.try_get(), Some(Outcome::PeerStopped(8)));

    // The same flow with the future API: an observation is directly awaitable
    // (its waker is registered and deregistered automatically; dropping the
    // future cancels cleanly). Driven here by hand instead of an executor.
    let mut peer_generation = space.subject(9).expect("fresh key");
    let mut future = space.observe(&9).expect("subject retained").into_future();
    let signal = Arc::new(Signal(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&signal));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    peer_generation.complete(Outcome::PeerStopped(9));
    while !signal.0.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(Outcome::PeerStopped(9))
    ));
}
