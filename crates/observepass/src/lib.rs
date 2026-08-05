//! Generation-safe completion publication and observation.

use core::hash::{BuildHasherDefault, Hash, Hasher};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;
#[cfg(loom)]
use std::sync::PoisonError;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fixed-seed 64-bit multiply-xor-rotate hasher (rustc's `FxHash`, as in the
/// `rustc-hash` crate). Deterministic across runs and fast for small keys;
/// not collision-hardened, so it is only used for the internal key table,
/// whose keys come from the embedding application rather than an adversary.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(
                chunk.try_into().expect("chunk is 8 bytes"),
            ));
        }
        let tail = chunks.remainder();
        if !tail.is_empty() {
            let mut padded = [0_u8; 8];
            padded[..tail.len()].copy_from_slice(tail);
            self.add(u64::from_le_bytes(padded));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

type BuildFx = BuildHasherDefault<FxHasher>;

#[cfg(loom)]
use loom::cell::UnsafeCell;
#[cfg(loom)]
use loom::sync::Arc;
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(loom)]
use loom::thread::{Thread, current, park};
#[cfg(not(loom))]
use parking_lot::Mutex;
#[cfg(not(loom))]
use std::cell::UnsafeCell;
#[cfg(not(loom))]
use std::sync::Arc;
#[cfg(not(loom))]
use std::thread::{Thread, current, park};

/// Acquire a mutex, unwrapping poisoning. std's `Mutex` poisons (and, on
/// macOS, lazily heap-allocates its pthread mutex on first lock);
/// `parking_lot`'s is inline and does not poison. The loom build models the
/// same lock protocol with loom's scheduler.
#[cfg(loom)]
fn lock<T>(mutex: &loom::sync::Mutex<T>) -> loom::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(recover)
}

#[cfg(not(loom))]
fn lock<T>(mutex: &parking_lot::Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(loom)]
fn recover<T>(error: PoisonError<T>) -> T {
    error.into_inner()
}

/// Completion-state bits for [`Slot`].
const COMPLETED: usize = 1 << 0;
const HAS_WAITER: usize = 1 << 1;

/// Per-subject completion cell: a lock-free outcome publication point plus a
/// mutex-protected waiter registry.
///
/// The full safety argument is recorded in `docs/research-log.md`
/// (EXPERIMENT 6). Summary of the invariants:
/// - The outcome cell is written exactly once, by `complete`, and the write
///   happens-before the COMPLETED bit is set by the Release `fetch_or`.
/// - The outcome cell is read only after observing COMPLETED via an
///   Acquire-or-stronger load of `state`, which synchronizes-with that
///   Release RMW.
/// - The `HAS_WAITER` bit and the `COMPLETED` bit share one word, so the two
///   RMWs are totally ordered by the modification order: a waiter that
///   completes its registration never parks without either seeing `COMPLETED`
///   on its post-push recheck or receiving an unpark token from the drain.
/// - Reclamation is entirely `Arc`-based; no raw pointer outlives the slot.
struct Slot<O> {
    state: AtomicUsize,
    outcome: UnsafeCell<Option<O>>,
    waiters: Mutex<Vec<Thread>>,
}

// SAFETY: the outcome cell is written exactly once, before the COMPLETED bit
// is published by the Release RMW, and read only after COMPLETED is observed
// (Acquire or stronger); while shared it is immutable. `O: Send + Sync`
// bounds the shared access to the stored value and its clones.
unsafe impl<O: Send + Sync> Sync for Slot<O> {}

impl<O> Slot<O> {
    /// Borrow the outcome cell.
    ///
    /// # Safety
    /// The caller must have observed the COMPLETED bit (Acquire or stronger);
    /// the single write happens-before that bit is set (Release RMW), so the
    /// cell holds a valid outcome.
    #[cfg(not(loom))]
    unsafe fn outcome_ref(&self) -> Option<&O> {
        // SAFETY: gated by the caller via the COMPLETED bit (invariant 2).
        unsafe { (*self.outcome.get()).as_ref() }
    }

    /// Borrow the outcome cell (loom-checked access).
    ///
    /// # Safety
    /// Same gating as the non-loom branch; loom additionally verifies the
    /// access against its scheduling model.
    #[cfg(loom)]
    unsafe fn outcome_ref(&self) -> Option<&O> {
        self.outcome.with(|ptr| {
            // SAFETY: same gating as the non-loom branch; loom additionally
            // verifies the access against its scheduling model.
            unsafe { (*ptr).as_ref() }
        })
    }

    /// Write the outcome cell.
    ///
    /// # Safety
    /// Called exactly once, before the COMPLETED bit is set.
    #[cfg(not(loom))]
    unsafe fn set_outcome(&self, outcome: O) {
        // SAFETY: single writer (invariant 1).
        unsafe { *self.outcome.get() = Some(outcome) };
    }

    /// Write the outcome cell (loom-checked access).
    ///
    /// # Safety
    /// Called exactly once, before the COMPLETED bit is set.
    #[cfg(loom)]
    unsafe fn set_outcome(&self, outcome: O) {
        self.outcome.with_mut(|ptr| {
            // SAFETY: single writer (invariant 1).
            unsafe { *ptr = Some(outcome) };
        });
    }

    /// Clear the outcome cell.
    ///
    /// # Safety
    /// No reader can observe the cell (a pooled slot has no observers; the
    /// write is published by the subsequent Release store of `state`).
    #[cfg(not(loom))]
    unsafe fn clear_outcome(&self) {
        // SAFETY: no concurrent readers (pooled-slot invariant).
        unsafe { *self.outcome.get() = None };
    }

    /// Clear the outcome cell (loom-checked access).
    ///
    /// # Safety
    /// No reader can observe the cell (a pooled slot has no observers; the
    /// write is published by the subsequent Release store of `state`).
    #[cfg(loom)]
    unsafe fn clear_outcome(&self) {
        self.outcome.with_mut(|ptr| {
            // SAFETY: no concurrent readers (pooled-slot invariant).
            unsafe { *ptr = None };
        });
    }

    /// Return a pooled slot to the pristine pending state for reuse by a new
    /// generation.
    fn reset(&self) {
        // SAFETY: pooled slots have no observers (see `Subject::drop`), so
        // the cell is unobservable; the Release store publishes the clear.
        unsafe { self.clear_outcome() };
        self.state.store(0, Ordering::Release);
    }
}

struct SlotEntry<O> {
    generation: usize,
    slot: Arc<Slot<O>>,
}

/// Upper bound on recycled slots retained per space.
const SLOT_POOL_CAP: usize = 128;

/// The key table plus the recycled-slot pool, guarded by one mutex so the
/// pool needs no lock of its own (a single `&mut` through the guard).
struct Entries<K, O> {
    map: HashMap<K, SlotEntry<O>, BuildFx>,
    // Recycled slots that no observer can still read, cap-bounded so
    // retention stays explicitly bounded. A pooled slot's strong count is
    // exactly the pool's own reference (see `Subject::drop`).
    pool: Vec<Arc<Slot<O>>>,
}

struct Inner<K, O> {
    // Monotonic generation source. `fetch_add` is a read-modify-write, so
    // every call observes a distinct value regardless of ordering; the value
    // is only compared under the `entries` mutex, whose acquire/release
    // orders the entry's publication. Relaxed is therefore sufficient.
    next_generation: AtomicUsize,
    entries: Mutex<Entries<K, O>>,
}

/// Shared completion namespace.
pub struct ObservationSpace<K, O> {
    inner: Arc<Inner<K, O>>,
}

impl<K, O> Clone for ObservationSpace<K, O> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K, O> Default for ObservationSpace<K, O> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, O> ObservationSpace<K, O> {
    /// Construct an empty observation namespace.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                next_generation: AtomicUsize::new(1),
                entries: Mutex::new(Entries {
                    map: HashMap::with_hasher(BuildFx::default()),
                    pool: Vec::new(),
                }),
            }),
        }
    }
}

/// A live subject already exists at this key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectExists<K>(pub K);

/// No retained subject exists at this key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSubject<K>(pub K);

impl<K, O> ObservationSpace<K, O>
where
    K: Eq + Hash + Clone,
{
    /// Register one new subject generation.
    ///
    /// # Errors
    /// Returns [`SubjectExists`] while the current generation remains retained.
    ///
    /// # Panics
    /// Panics if the process exhausts all generations.
    pub fn subject(&self, key: K) -> Result<Subject<K, O>, SubjectExists<K>> {
        let mut entries = lock(&self.inner.entries);
        let pooled = entries.pool.pop();
        match entries.map.entry(key.clone()) {
            Entry::Occupied(_) => {
                // Restore the unused pooled slot.
                if let Some(slot) = pooled {
                    entries.pool.push(slot);
                }
                Err(SubjectExists(key))
            }
            Entry::Vacant(vacant) => {
                let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
                assert_ne!(generation, usize::MAX, "observation generation exhausted");
                let slot = match pooled {
                    Some(slot) => {
                        slot.reset();
                        slot
                    }
                    None => Arc::new(Slot {
                        state: AtomicUsize::new(0),
                        outcome: UnsafeCell::new(None),
                        waiters: Mutex::new(Vec::new()),
                    }),
                };
                vacant.insert(SlotEntry {
                    generation,
                    slot: slot.clone(),
                });
                Ok(Subject {
                    inner: self.inner.clone(),
                    key,
                    generation,
                    slot,
                    completed: false,
                })
            }
        }
    }

    /// Observe the current generation, including an already completed one.
    ///
    /// # Errors
    /// Returns [`UnknownSubject`] when no generation is currently retained.
    pub fn observe(&self, key: &K) -> Result<Observation<O>, UnknownSubject<K>> {
        let entries = lock(&self.inner.entries);
        let slot = entries
            .map
            .get(key)
            .map(|entry| entry.slot.clone())
            .ok_or_else(|| UnknownSubject(key.clone()))?;
        Ok(Observation { slot })
    }
}

/// Publisher and retention owner for one exact generation.
pub struct Subject<K, O>
where
    K: Eq + Hash,
{
    inner: Arc<Inner<K, O>>,
    key: K,
    generation: usize,
    slot: Arc<Slot<O>>,
    completed: bool,
}

impl<K, O> Subject<K, O>
where
    K: Eq + Hash,
{
    /// Publish the terminal outcome exactly once.
    ///
    /// # Panics
    /// Panics when the same subject publishes completion more than once.
    pub fn complete(&mut self, outcome: O) {
        assert!(!self.completed, "subject completed twice");
        // SAFETY: single writer (invariant 1); the Release RMW below
        // publishes the write to readers that observe COMPLETED.
        unsafe { self.slot.set_outcome(outcome) };
        self.completed = true;
        let previous = self.slot.state.fetch_or(COMPLETED, Ordering::Release);
        if previous & HAS_WAITER != 0 {
            let waiters = {
                let mut waiters = lock(&self.slot.waiters);
                mem::take(&mut *waiters)
            };
            for waiter in waiters {
                waiter.unpark();
            }
        }
    }
}

impl<K, O> Drop for Subject<K, O>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        let mut entries = lock(&self.inner.entries);
        if entries
            .map
            .get(&self.key)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            entries.map.remove(&self.key);
            // With the entry gone and the entries lock held, no new observer
            // can reference this slot (`observe` needs both), so the strong
            // count can only decrease from here: it is exactly 1 (our own
            // handle) exactly when no observer still holds it, making the
            // slot safe to recycle.
            if Arc::strong_count(&self.slot) == 1 && entries.pool.len() < SLOT_POOL_CAP {
                entries.pool.push(self.slot.clone());
            }
        }
    }
}

/// A cancellable observation of one captured subject generation.
pub struct Observation<O> {
    slot: Arc<Slot<O>>,
}

impl<O: Clone> Observation<O> {
    /// Return the outcome without blocking when already complete.
    ///
    /// # Panics
    /// Panics only if the completed-bit protocol is violated (a programmer
    /// bug); a slot that reports completion always holds an outcome.
    #[must_use]
    pub fn try_get(&self) -> Option<O> {
        if self.slot.state.load(Ordering::Acquire) & COMPLETED != 0 {
            // SAFETY: COMPLETED observed with Acquire (invariant 2).
            Some(
                unsafe { self.slot.outcome_ref() }
                    .expect("completed slot holds an outcome")
                    .clone(),
            )
        } else {
            None
        }
    }

    /// Block the current thread until completion.
    ///
    /// The waiter registers under the waiters mutex, re-checks completion,
    /// and only then parks. A completion that raced the registration either
    /// publishes before the post-push recheck (seen without parking) or sees
    /// the `HAS_WAITER` bit and drains the registration, so the park consumes
    /// the unpark token and returns immediately. Nothing that could itself
    /// park runs between registration and [`park`], so the token cannot be
    /// consumed elsewhere.
    ///
    /// # Panics
    /// Panics only if the completed-bit protocol is violated (a programmer
    /// bug); a slot that reports completion always holds an outcome.
    #[must_use]
    pub fn wait(&self) -> O {
        loop {
            if self.slot.state.load(Ordering::Acquire) & COMPLETED != 0 {
                // SAFETY: COMPLETED observed with Acquire (invariant 2), so
                // the outcome is present.
                return unsafe { self.slot.outcome_ref() }
                    .expect("completed slot holds an outcome")
                    .clone();
            }
            self.slot.state.fetch_or(HAS_WAITER, Ordering::SeqCst);
            let mut waiters = lock(&self.slot.waiters);
            waiters.push(current());
            if self.slot.state.load(Ordering::SeqCst) & COMPLETED != 0 {
                // SAFETY: COMPLETED observed with SeqCst (invariant 2), so the
                // outcome is present. We may still be registered; a later
                // drain only produces a spurious token, consumed by our next
                // park.
                return unsafe { self.slot.outcome_ref() }
                    .expect("completed slot holds an outcome")
                    .clone();
            }
            drop(waiters);
            park();
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests;
#[cfg(all(test, not(loom)))]
mod tests;
