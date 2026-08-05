//! Generation-safe completion publication and observation.

use core::hash::{BuildHasherDefault, Hash, Hasher};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;
#[cfg(loom)]
use std::sync::PoisonError;
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
use loom::sync::Arc;
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(loom)]
use loom::thread::{Thread, current, park};
#[cfg(not(loom))]
use parking_lot::Mutex;
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

/// Per-subject completion cell: the outcome plus every thread blocked in
/// [`Observation::wait`] on this generation.
///
/// Registration and publication happen under the same mutex, so they cannot
/// interleave. A waiter registered before completion is guaranteed to wake:
/// `unpark` makes the thread's token available, and `park` consumes an
/// already-available token without blocking (std token semantics). Waiters
/// are drained and unparked after releasing the lock.
struct Slot<O> {
    state: Mutex<SlotState<O>>,
}

struct SlotState<O> {
    outcome: Option<O>,
    waiters: Vec<Thread>,
}

struct SlotEntry<O> {
    generation: usize,
    slot: Arc<Slot<O>>,
}

struct Inner<K, O> {
    // Monotonic generation source. `fetch_add` is a read-modify-write, so
    // every call observes a distinct value regardless of ordering; the value
    // is only compared under the `entries` mutex, whose acquire/release
    // orders the entry's publication. Relaxed is therefore sufficient.
    next_generation: AtomicUsize,
    entries: Mutex<HashMap<K, SlotEntry<O>, BuildFx>>,
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
                entries: Mutex::new(HashMap::with_hasher(BuildFx::default())),
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
        match entries.entry(key.clone()) {
            Entry::Occupied(_) => Err(SubjectExists(key)),
            Entry::Vacant(vacant) => {
                let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
                assert_ne!(generation, usize::MAX, "observation generation exhausted");
                let slot = Arc::new(Slot {
                    state: Mutex::new(SlotState {
                        outcome: None,
                        waiters: Vec::new(),
                    }),
                });
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
        let mut state = lock(&self.slot.state);
        state.outcome = Some(outcome);
        self.completed = true;
        let waiters = mem::take(&mut state.waiters);
        drop(state);
        for waiter in waiters {
            waiter.unpark();
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
            .get(&self.key)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            entries.remove(&self.key);
        }
    }
}

/// A cancellable observation of one captured subject generation.
pub struct Observation<O> {
    slot: Arc<Slot<O>>,
}

impl<O: Clone> Observation<O> {
    /// Return the outcome without blocking when already complete.
    #[must_use]
    pub fn try_get(&self) -> Option<O> {
        lock(&self.slot.state).outcome.clone()
    }

    /// Block the current thread until completion.
    ///
    /// The waiter registers its thread handle under the slot mutex, re-checks
    /// the outcome, and only then parks. A completion that raced the
    /// registration either publishes before the re-check (seen without
    /// parking) or drains the registration and sets the unpark token (the
    /// park then returns immediately). Nothing that could itself park runs
    /// between registration and [`park`], so the token cannot be consumed
    /// elsewhere.
    #[must_use]
    pub fn wait(&self) -> O {
        let mut state = lock(&self.slot.state);
        loop {
            if let Some(outcome) = state.outcome.clone() {
                return outcome;
            }
            state.waiters.push(current());
            drop(state);
            park();
            state = lock(&self.slot.state);
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests;
#[cfg(all(test, not(loom)))]
mod tests;
