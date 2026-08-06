//! Shared probes: outcome types that count their drops and wakers that
//! record every wake, so tests can assert exact destruction and
//! notification counts.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

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
///
/// Hand-rolled `RawWaker` with a single static vtable: the std
/// `Wake`-derived vtable is a const-promoted temporary whose address
/// differs between code sites under Miri, which makes `will_wake`
/// spuriously false there (the production suite documents the same
/// workaround). One static vtable keeps `will_wake` reliable under Miri
/// and identical in behavior to the std derive everywhere else.
#[derive(Debug, Default)]
pub struct CountWake {
    wakes: AtomicUsize,
}

static COUNT_WAKE_VTABLE: RawWakerVTable = RawWakerVTable::new(
    count_wake_clone,
    count_wake_wake,
    count_wake_wake_by_ref,
    count_wake_drop,
);

unsafe fn count_wake_clone(data: *const ()) -> RawWaker {
    // SAFETY: `data` is a live `Arc<CountWake>` pointer owned by the waker
    // being cloned; the ManuallyDrop borrow is forgotten, so the refcount
    // gains exactly one for the returned RawWaker.
    let probe = unsafe { Arc::<CountWake>::from_raw(data.cast::<CountWake>()) };
    let cloned = Arc::clone(&probe);
    std::mem::forget(probe);
    RawWaker::new(Arc::into_raw(cloned).cast::<()>(), &COUNT_WAKE_VTABLE)
}

unsafe fn count_wake_wake(data: *const ()) {
    // SAFETY: `data` is an owned `Arc<CountWake>` pointer; reconstruct and
    // drop it after use.
    let probe = unsafe { Arc::<CountWake>::from_raw(data.cast::<CountWake>()) };
    probe.wakes.fetch_add(1, Ordering::SeqCst);
}

unsafe fn count_wake_wake_by_ref(data: *const ()) {
    // SAFETY: `data` is a borrowed `Arc<CountWake>` pointer; the
    // ManuallyDrop borrow is forgotten so the refcount is unchanged.
    let probe = unsafe { Arc::<CountWake>::from_raw(data.cast::<CountWake>()) };
    probe.wakes.fetch_add(1, Ordering::SeqCst);
    std::mem::forget(probe);
}

unsafe fn count_wake_drop(data: *const ()) {
    // SAFETY: `data` is an owned `Arc<CountWake>` pointer being released.
    drop(unsafe { Arc::<CountWake>::from_raw(data.cast::<CountWake>()) });
}

impl CountWake {
    pub fn waker() -> (Waker, Arc<Self>) {
        let probe = Arc::new(Self::default());
        let raw = RawWaker::new(
            Arc::into_raw(Arc::clone(&probe)).cast::<()>(),
            &COUNT_WAKE_VTABLE,
        );
        // SAFETY: the vtable functions implement the Arc refcount protocol
        // exactly (clone increments, wake consumes, wake_by_ref borrows,
        // drop releases) and the data pointer is a live `Arc<CountWake>`.
        let waker = unsafe { Waker::from_raw(raw) };
        (waker, probe)
    }

    pub fn count(&self) -> usize {
        self.wakes.load(Ordering::SeqCst)
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
