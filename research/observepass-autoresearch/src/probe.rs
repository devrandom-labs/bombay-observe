//! Shared probes: outcome types that count their drops and wakers that
//! record every wake, so tests can assert exact destruction and
//! notification counts.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;

/// An outcome whose destruction is observable. `Clone` clones share the
/// same drop counter; every clone's drop increments it exactly once.
#[derive(Debug)]
pub struct DropProbe {
    counter: Arc<AtomicUsize>,
    /// Distinct payload so tests can tell values apart.
    pub tag: u64,
}

impl DropProbe {
    pub fn new(tag: u64) -> (Self, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        (
            Self {
                counter: Arc::clone(&counter),
                tag,
            },
            counter,
        )
    }
}

impl Clone for DropProbe {
    fn clone(&self) -> Self {
        Self {
            counter: Arc::clone(&self.counter),
            tag: self.tag,
        }
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

/// A waker that counts every `wake`/`wake_by_ref`, for asserting exact
/// notification counts across cancellation and completion.
#[derive(Debug, Default)]
pub struct CountWake {
    wakes: AtomicUsize,
}

impl CountWake {
    pub fn waker() -> (std::task::Waker, Arc<Self>) {
        let probe = Arc::new(Self::default());
        let waker = std::task::Waker::from(Arc::clone(&probe) as Arc<Self>);
        (waker, probe)
    }

    pub fn count(&self) -> usize {
        self.wakes.load(Ordering::SeqCst)
    }
}

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

/// Poll a future once with the given waker.
pub fn poll_once<O: Clone>(
    future: std::pin::Pin<&mut observepass::ObservationFuture<O>>,
    waker: &std::task::Waker,
) -> std::task::Poll<O> {
    let mut cx = std::task::Context::from_waker(waker);
    future.poll(&mut cx)
}
