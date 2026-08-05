//! Generation-safe completion publication and observation.

use core::hash::{BuildHasherDefault, Hash, Hasher};
#[cfg(loom)]
use loom::sync::atomic::AtomicUsize;
use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::mem::{self, MaybeUninit};
use std::pin::Pin;
use std::ptr;
#[cfg(loom)]
use std::sync::PoisonError;
#[cfg(not(loom))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::Duration;
#[cfg(not(loom))]
use std::time::Instant;

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
use loom::sync::RwLock;
#[cfg(loom)]
use loom::sync::atomic::AtomicPtr;
#[cfg(loom)]
use loom::thread::{Thread, current, park};
#[cfg(not(loom))]
use parking_lot::Mutex;
#[cfg(not(loom))]
use parking_lot::RwLock;
#[cfg(not(loom))]
use std::cell::UnsafeCell;
#[cfg(not(loom))]
use std::sync::atomic::AtomicPtr;
#[cfg(not(loom))]
use std::thread::{Thread, current, park, park_timeout};
#[cfg(not(loom))]
use triomphe::Arc;

/// Acquire a mutex, unwrapping poisoning. std's `Mutex` poisons (and, on
/// macOS, lazily heap-allocates its pthread mutex on first lock);
/// `parking_lot`'s is inline and does not poison. The loom build models the
/// same lock protocol with loom's scheduler.
#[cfg(loom)]
fn lock<T>(mutex: &loom::sync::Mutex<T>) -> loom::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(recover)
}

#[cfg(loom)]
fn read_lock<T>(lock: &loom::sync::RwLock<T>) -> loom::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(recover)
}

#[cfg(loom)]
fn write_lock<T>(lock: &loom::sync::RwLock<T>) -> loom::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(recover)
}

#[cfg(not(loom))]
fn lock<T>(mutex: &parking_lot::Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

/// Acquire the entries lock for reading (observe).
#[cfg(not(loom))]
fn read_lock<T>(lock: &parking_lot::RwLock<T>) -> parking_lot::RwLockReadGuard<'_, T> {
    lock.read()
}

/// Acquire the entries lock for writing (subject/retire).
#[cfg(not(loom))]
fn write_lock<T>(lock: &parking_lot::RwLock<T>) -> parking_lot::RwLockWriteGuard<'_, T> {
    lock.write()
}

#[cfg(loom)]
fn recover<T>(error: PoisonError<T>) -> T {
    error.into_inner()
}

/// Park until woken or the deadline passes; returns whether the deadline has
/// not yet passed. Under loom, park without a timeout (the model has no
/// clock, and the registration protocol is what matters).
#[cfg(not(loom))]
fn park_until(deadline: Instant) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    park_timeout(deadline - now);
    true
}

/// Park until woken (loom variant; no clock).
#[cfg(loom)]
fn park_until() -> bool {
    park();
    true
}

/// Completion-state bits for [`Slot`].
const COMPLETED: usize = 1 << 0;
const HAS_WAITER: usize = 1 << 1;
/// Set exactly while the outcome cell holds a live value. Rides the state
/// word (set by `complete`'s RMW, cleared by `reset`'s recycle and
/// `into_outcome`'s take) so the outcome cell needs no separate tag: the
/// validity gate is `COMPLETED` for readers, `OUTCOME_VALID` for droppers.
const OUTCOME_VALID: usize = 1 << 2;

/// A registered waiter: either a blocked thread (sync [`Observation::wait`])
/// or an async waker ([`Observation::register_waker`]).
enum Waiter {
    Thread(Thread),
    Waker(Waker),
}

/// Per-subject completion cell: a lock-free outcome publication point plus a
/// mutex-protected waiter registry.
///
/// The full safety argument is recorded in `docs/research-log.md`
/// (EXPERIMENT 6). Summary of the invariants:
/// - The outcome cell is written exactly once, by `complete`, and the write
///   happens-before the `COMPLETED|OUTCOME_VALID` bits are set by the Release
///   `fetch_or`.
/// - The outcome cell is read only after observing COMPLETED via an
///   Acquire-or-stronger load of `state`, which synchronizes-with that
///   Release RMW.
/// - The outcome cell is dropped exactly once: by `reset` (a pooled slot has
///   no observers) or by `into_outcome`'s take (which clears `OUTCOME_VALID`,
///   so the slot's final drop skips it).
/// - The `HAS_WAITER` bit and the `COMPLETED` bit share one word, so the two
///   RMWs are totally ordered by the modification order: a waiter that
///   completes its registration never parks without either seeing `COMPLETED`
///   on its post-push recheck or receiving an unpark token from the drain.
/// - Reclamation is entirely `Arc`-based; no raw pointer outlives the slot.
struct Slot<O> {
    state: AtomicUsize,
    // O-sized (no Option tag): the OUTCOME_VALID bit in `state` is the
    // liveness marker, and COMPLETED gates every read.
    outcome: UnsafeCell<MaybeUninit<O>>,
    // Raw pointer to a lazily created `Mutex<Vec<Waiter>>` (null until the
    // first waiter). The slot owns the allocation and reclaims it at its
    // final drop, so the common case (no waiters ever) keeps the slot 8
    // bytes smaller and allocation-free; the hot path never touches this
    // field. The Arc indirection is unnecessary: the mutex is owned by the
    // slot itself, which outlives every waiter.
    waiters: AtomicPtr<Mutex<Vec<Waiter>>>,
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
    unsafe fn outcome_ref(&self) -> &O {
        // SAFETY: gated by the caller via the COMPLETED bit (invariant 2).
        unsafe { (*self.outcome.get()).assume_init_ref() }
    }

    /// Borrow the outcome cell (loom-checked access).
    ///
    /// # Safety
    /// Same gating as the non-loom branch; loom additionally verifies the
    /// access against its scheduling model.
    #[cfg(loom)]
    unsafe fn outcome_ref(&self) -> &O {
        self.outcome.with(|ptr| {
            // SAFETY: same gating as the non-loom branch; loom additionally
            // verifies the access against its scheduling model.
            unsafe { (*ptr).assume_init_ref() }
        })
    }

    /// Write the outcome cell.
    ///
    /// # Safety
    /// Called exactly once, before the COMPLETED bit is set.
    #[cfg(not(loom))]
    unsafe fn set_outcome(&self, outcome: O) {
        // SAFETY: single writer (invariant 1); the write happens-before the
        // COMPLETED|OUTCOME_VALID Release RMW.
        unsafe { (*self.outcome.get()).write(outcome) };
    }

    /// Write the outcome cell (loom-checked access).
    ///
    /// # Safety
    /// Called exactly once, before the COMPLETED bit is set.
    #[cfg(loom)]
    unsafe fn set_outcome(&self, outcome: O) {
        self.outcome.with_mut(|ptr| {
            // SAFETY: single writer (invariant 1).
            unsafe { (*ptr).write(outcome) };
        });
    }

    /// Drop the published outcome, if the cell holds one.
    ///
    /// # Safety
    /// No reader can observe the cell (a pooled slot has no observers; the
    /// subsequent store of `state` publishes the drop), and the caller
    /// guarantees `OUTCOME_VALID` is set (so the drop happens exactly once).
    #[cfg(not(loom))]
    unsafe fn drop_outcome(&self) {
        // SAFETY: no concurrent readers (pooled-slot invariant).
        unsafe { (*self.outcome.get()).assume_init_drop() };
    }

    /// Drop the published outcome (loom-checked access).
    ///
    /// # Safety
    /// Same as the non-loom branch.
    #[cfg(loom)]
    unsafe fn drop_outcome(&self) {
        self.outcome.with_mut(|ptr| {
            // SAFETY: same as the non-loom branch.
            unsafe { (*ptr).assume_init_drop() };
        });
    }

    /// The waiters registry, created on first use.
    ///
    /// The registry is a `Box<Mutex<Vec<Waiter>>>` published by a CAS from
    /// null on the first access and reclaimed by the slot's final drop.
    /// Losing the init race reclaims the loser's box; the winner's pointer
    /// is live as long as the slot is.
    fn waiters(&self) -> &Mutex<Vec<Waiter>> {
        let ptr = self.waiters.load(Ordering::Acquire);
        if !ptr.is_null() {
            // SAFETY: a non-null pointer was published by the init CAS and
            // is reclaimed only by this slot's final drop, which cannot run
            // while we hold a borrow of the slot.
            return unsafe { &*ptr };
        }
        // SAFETY: `new` is a fresh, uniquely owned allocation.
        let new = Box::into_raw(Box::new(Mutex::new(Vec::new())));
        match self.waiters.compare_exchange(
            ptr::null_mut(),
            new,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: we won the init race; `new` is published.
                unsafe { &*new }
            }
            Err(actual) => {
                // Lost the init race: reclaim our box and use the winner's.
                // SAFETY: `new` was never published; we own it exclusively.
                drop(unsafe { Box::from_raw(new) });
                // SAFETY: `actual` was published by the winner and is live
                // as long as the slot is.
                unsafe { &*actual }
            }
        }
    }

    /// Return a pooled slot to the pristine pending state for reuse by a new
    /// generation.
    fn reset(&self) {
        let state = self.state.load(Ordering::Relaxed);
        if state & OUTCOME_VALID != 0 {
            // SAFETY: pooled slots have no observers (see `Subject::drop`),
            // so the cell is unobservable; the Release store below publishes
            // the drop. OUTCOME_VALID is then cleared by the same store, so
            // the slot's final drop will not double-drop.
            unsafe { self.drop_outcome() };
        }
        if state & HAS_WAITER != 0 {
            // A stale registration can survive (a waker whose caller dropped
            // its observation before completion). Drain it so no dead waiter
            // is retained in the pool or fired across generations. No waiter
            // can be in flight: a live waiter holds an observation Arc, and
            // pooled slots have none.
            let ptr = self.waiters.load(Ordering::Acquire);
            if !ptr.is_null() {
                // SAFETY: HAS_WAITER implies a waiter registered, which
                // initialized the registry; the pointer is live as long as
                // the slot is.
                unsafe { lock(&*ptr).clear() };
            }
        }
        self.state.store(0, Ordering::Release);
    }
}

impl<O> Drop for Slot<O> {
    fn drop(&mut self) {
        // SAFETY: the last Arc reference is being dropped (reclamation is
        // entirely Arc-based), so no other thread can access the slot.
        // OUTCOME_VALID is set exactly while the cell holds a live value:
        // written by complete, cleared by reset's recycle and into_outcome's
        // take, so the drop fires exactly once.
        if self.state.load(Ordering::Relaxed) & OUTCOME_VALID != 0 {
            // SAFETY: see above.
            unsafe { self.drop_outcome() };
        }
        // Reclaim the lazily created waiters registry, if any.
        let ptr = self.waiters.load(Ordering::Relaxed);
        if !ptr.is_null() {
            // SAFETY: the last reference is being dropped; the box is owned
            // by this slot and no other thread can access it.
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

struct SlotEntry<O> {
    generation: usize,
    slot: Arc<Slot<O>>,
}

/// Upper bound on recycled slots retained per space.
const SLOT_POOL_CAP: usize = 128;

/// Number of inline key entries before promoting to a hash map.
const INLINE_CAP: usize = 4;

/// Key-table storage: an inline vector of `(key, entry)` pairs for the
/// common transient case (few live generations at once), promoting to a
/// hash map at [`INLINE_CAP`] entries so retention-scale workloads stay
/// O(1). The inline path avoids hashing and probing entirely.
enum SmallMap<K, O> {
    Inline(Vec<(K, SlotEntry<O>)>),
    Hash(HashMap<K, SlotEntry<O>, BuildFx>),
}

impl<K, O> Default for SmallMap<K, O> {
    fn default() -> Self {
        Self::Inline(Vec::new())
    }
}

impl<K: Eq + Hash, O> SmallMap<K, O> {
    fn get(&self, key: &K) -> Option<&SlotEntry<O>> {
        match self {
            Self::Inline(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, e)| e),
            Self::Hash(map) => map.get(key),
        }
    }

    /// Whether `key` has no entry.
    fn is_vacant(&self, key: &K) -> bool {
        match self {
            Self::Inline(entries) => !entries.iter().any(|(k, _)| k == key),
            Self::Hash(map) => !map.contains_key(key),
        }
    }

    /// Insert at `key`; the caller guarantees the key is vacant.
    fn insert_vacant(&mut self, key: K, entry: SlotEntry<O>) {
        match self {
            Self::Inline(entries) => {
                if entries.len() >= INLINE_CAP {
                    // At most INLINE_CAP entries are promoted, so the hash
                    // table never needs more than INLINE_CAP * 2 capacity.
                    let mut map =
                        HashMap::with_capacity_and_hasher(INLINE_CAP * 2, BuildFx::default());
                    map.extend(entries.drain(..));
                    map.insert(key, entry);
                    *self = Self::Hash(map);
                } else {
                    entries.push((key, entry));
                }
            }
            Self::Hash(map) => {
                map.insert(key, entry);
            }
        }
    }

    /// Remove the entry at `key` iff it is exactly `generation`.
    fn remove_if(&mut self, key: &K, generation: usize) -> bool {
        match self {
            Self::Inline(entries) => {
                if let Some(index) = entries.iter().position(|(k, _)| k == key)
                    && entries[index].1.generation == generation
                {
                    entries.swap_remove(index);
                    return true;
                }
                false
            }
            Self::Hash(map) => {
                if map
                    .get(key)
                    .is_some_and(|entry| entry.generation == generation)
                {
                    map.remove(key);
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// The key table plus the recycled-slot pool, guarded by one mutex so the
/// pool needs no lock of its own (a single `&mut` through the guard).
struct Entries<K, O> {
    map: SmallMap<K, O>,
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
    entries: RwLock<Entries<K, O>>,
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
                entries: RwLock::new(Entries {
                    map: SmallMap::default(),
                    pool: Vec::new(),
                }),
            }),
        }
    }
}

/// A live subject already exists at this key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a subject already exists for key {0:?}")]
pub struct SubjectExists<K>(pub K);

/// No retained subject exists at this key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no subject retained for key {0:?}")]
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
        let mut entries = write_lock(&self.inner.entries);
        let pooled = entries.pool.pop();
        if !entries.map.is_vacant(&key) {
            // Restore the unused pooled slot.
            if let Some(slot) = pooled {
                entries.pool.push(slot);
            }
            return Err(SubjectExists(key));
        }
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        assert_ne!(generation, usize::MAX, "observation generation exhausted");
        let slot = match pooled {
            Some(slot) => {
                slot.reset();
                slot
            }
            None => Arc::new(Slot {
                state: AtomicUsize::new(0),
                outcome: UnsafeCell::new(MaybeUninit::uninit()),
                waiters: AtomicPtr::new(ptr::null_mut()),
            }),
        };
        entries.map.insert_vacant(
            key.clone(),
            SlotEntry {
                generation,
                slot: slot.clone(),
            },
        );
        Ok(Subject {
            inner: self.inner.clone(),
            key,
            generation,
            slot,
            completed: false,
        })
    }

    /// Observe the current generation, including an already completed one.
    ///
    /// # Errors
    /// Returns [`UnknownSubject`] when no generation is currently retained.
    pub fn observe(&self, key: &K) -> Result<Observation<O>, UnknownSubject<K>> {
        let entries = read_lock(&self.inner.entries);
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
        let previous = self
            .slot
            .state
            .fetch_or(COMPLETED | OUTCOME_VALID, Ordering::Release);
        if previous & HAS_WAITER != 0 {
            let waiters = {
                let mut waiters = lock(self.slot.waiters());
                mem::take(&mut *waiters)
            };
            for waiter in waiters {
                match waiter {
                    Waiter::Thread(thread) => thread.unpark(),
                    Waiter::Waker(waker) => waker.wake(),
                }
            }
        }
    }
}

impl<K, O> Drop for Subject<K, O>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        let mut entries = write_lock(&self.inner.entries);
        if entries.map.remove_if(&self.key, self.generation) {
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
            Some(unsafe { self.slot.outcome_ref() }.clone())
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
                return unsafe { self.slot.outcome_ref() }.clone();
            }
            self.slot.state.fetch_or(HAS_WAITER, Ordering::SeqCst);
            let mut waiters = lock(self.slot.waiters());
            let thread_id = current().id();
            if !matches!(waiters.last(), Some(Waiter::Thread(t)) if t.id() == thread_id) {
                waiters.push(Waiter::Thread(current()));
            }
            if self.slot.state.load(Ordering::SeqCst) & COMPLETED != 0 {
                // SAFETY: COMPLETED observed with SeqCst (invariant 2), so the
                // outcome is present. Deregister: we are in the Vec, and a
                // stale registration must not survive into a pooled slot.
                waiters
                    .retain(|waiter| !matches!(waiter, Waiter::Thread(t) if t.id() == thread_id));
                return unsafe { self.slot.outcome_ref() }.clone();
            }
            drop(waiters);
            park();
        }
    }

    /// Block the current thread until completion or `timeout` elapses.
    ///
    /// Returns `None` when the timeout elapses before the outcome is
    /// published. A completion that races the deadline is still observed:
    /// the loop re-checks after every wake, before the deadline test. A
    /// timed-out waiter is deregistered before returning, so a later
    /// completion cannot wake it.
    ///
    /// # Panics
    /// Panics if the completed-bit protocol is violated (a programmer bug; a
    /// slot that reports completion always holds an outcome), or if `timeout`
    /// overflows the [`Instant`] deadline.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<O> {
        #[cfg(not(loom))]
        let deadline = Instant::now()
            .checked_add(timeout)
            .expect("wait timeout overflows Instant");
        #[cfg(loom)]
        let _ = timeout;
        loop {
            if self.slot.state.load(Ordering::Acquire) & COMPLETED != 0 {
                // SAFETY: COMPLETED observed with Acquire (invariant 2), so
                // the outcome is present.
                return Some(unsafe { self.slot.outcome_ref() }.clone());
            }
            self.slot.state.fetch_or(HAS_WAITER, Ordering::SeqCst);
            let mut waiters = lock(self.slot.waiters());
            let thread_id = current().id();
            if !matches!(waiters.last(), Some(Waiter::Thread(t)) if t.id() == thread_id) {
                waiters.push(Waiter::Thread(current()));
            }
            if self.slot.state.load(Ordering::SeqCst) & COMPLETED != 0 {
                // SAFETY: COMPLETED observed with SeqCst (invariant 2), so
                // the outcome is present. Deregister: we are in the Vec, and
                // a stale registration must not survive into a pooled slot.
                waiters
                    .retain(|waiter| !matches!(waiter, Waiter::Thread(t) if t.id() == thread_id));
                return Some(unsafe { self.slot.outcome_ref() }.clone());
            }
            drop(waiters);
            #[cfg(not(loom))]
            {
                if !park_until(deadline) {
                    // Deregister: the deadline passed while we were
                    // registered.
                    let mut waiters = lock(self.slot.waiters());
                    waiters.retain(
                        |waiter| !matches!(waiter, Waiter::Thread(t) if t.id() == thread_id),
                    );
                    return None;
                }
            }
            #[cfg(loom)]
            {
                park_until();
            }
        }
    }
}

impl<O> Observation<O> {
    /// Consume this observation and return the outcome by value, if the
    /// outcome is published and this handle is the last reference to the
    /// slot (no other observation, waiter, or the subject still holds it).
    ///
    /// Unlike [`Observation::try_get`], this supports outcomes that are not
    /// `Clone`: the value moves out of the slot. It returns `None` while the
    /// slot is still shared or the outcome is not yet published.
    #[must_use]
    pub fn into_outcome(self) -> Option<O> {
        let slot = Arc::try_unwrap(self.slot).ok()?;
        // Exclusive ownership via the move; no access can race it.
        if slot.state.load(Ordering::Acquire) & COMPLETED != 0 {
            // SAFETY: COMPLETED observed with Acquire (invariant 2), so the
            // cell was written before the Release RMW that set it.
            #[cfg(not(loom))]
            let outcome = unsafe { slot.outcome.get().read().assume_init() };
            #[cfg(loom)]
            let outcome = slot.outcome.with(|ptr| {
                // SAFETY: same gating as the non-loom branch; loom verifies
                // the access against its scheduling model.
                unsafe { (*ptr).assume_init_read() }
            });
            // Mark the value taken so the slot's Drop does not double-drop.
            slot.state.fetch_and(!OUTCOME_VALID, Ordering::Relaxed);
            Some(outcome)
        } else {
            None
        }
    }

    /// Register a waker to be woken when the outcome is published, for
    /// async adapters. Returns `true` when the outcome is already published
    /// (in which case nothing is registered and the caller can read the
    /// outcome directly).
    ///
    /// The waker may be woken spuriously and more than once; callers must
    /// re-read the outcome (via [`Observation::try_get`] or
    /// [`Observation::into_outcome`]) after a wake.
    #[must_use]
    pub fn register_waker(&self, waker: &Waker) -> bool {
        if self.slot.state.load(Ordering::Acquire) & COMPLETED != 0 {
            return true;
        }
        self.slot.state.fetch_or(HAS_WAITER, Ordering::SeqCst);
        let mut waiters = lock(self.slot.waiters());
        if self.slot.state.load(Ordering::SeqCst) & COMPLETED != 0 {
            // We may still be registered; a later drain produces only a
            // spurious wake, which callers must tolerate.
            return true;
        }
        // Registering a waker that will_wake an already-registered one is
        // idempotent (the std-endorsed pattern behind `Waker::clone_from`):
        // repeated polling of the same task never accumulates duplicates,
        // and the re-registration path performs no clone and no allocation.
        if waiters
            .iter()
            .any(|waiter| matches!(waiter, Waiter::Waker(existing) if existing.will_wake(waker)))
        {
            return false;
        }
        waiters.push(Waiter::Waker(waker.clone()));
        false
    }
}

/// A future that resolves to the outcome when the subject completes.
///
/// Created via [`Observation`]'s [`IntoFuture`] impl (the `.await` sugar in
/// an async adapter). Polling registers the task's waker idempotently
/// (repeated polls never accumulate duplicate registrations); dropping the
/// future before completion deregisters its waker, so cancelled
/// observations leave no stale registration that would keep the waker's
/// payload alive or fire across generations.
pub struct ObservationFuture<O> {
    observation: Observation<O>,
    // Every distinct waker this future has polled with. A task can migrate
    // between executors, registering a new waker each time (`will_wake`
    // dedup only folds identical wakers), and cancellation must deregister
    // all of them - remembering only the latest would leave earlier wakers
    // registered to be fired after the future is dropped.
    wakers: Vec<Waker>,
}

impl<O: Clone> Future for ObservationFuture<O> {
    type Output = O;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<O> {
        // SAFETY: `ObservationFuture` is not `Unpin` when `O` is not (the
        // triomphe Arc's `PhantomData<Slot<O>>` propagates the bound), but
        // its fields are never moved out of the pinned location: the
        // observation is only borrowed, and the waker slot is replaced in
        // place. This is the standard manual pin-projection for !Unpin
        // futures; the future's `Drop` also runs fine on a pinned value.
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(outcome) = this.observation.try_get() {
            return Poll::Ready(outcome);
        }
        // Idempotently register the task's waker; `register_waker` re-checks
        // completion, so a completion racing the registration is caught.
        let _ = this.observation.register_waker(cx.waker());
        if let Some(outcome) = this.observation.try_get() {
            return Poll::Ready(outcome);
        }
        // Track this waker for cancellation, deduplicated exactly like the
        // registration: repeated polling of the same task stays one entry,
        // a migrated task accumulates its distinct wakers.
        if !this.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            this.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl<O> Drop for ObservationFuture<O> {
    fn drop(&mut self) {
        // Deregister every waker this future registered: cancelling the
        // future must not leave a stale registration (including one from a
        // pre-migration waker) behind.
        if self.wakers.is_empty() {
            return;
        }
        let mut waiters = lock(self.observation.slot.waiters());
        waiters.retain(|waiter| {
            !matches!(waiter, Waiter::Waker(w) if self.wakers.iter().any(|mine| mine.will_wake(w)))
        });
    }
}

impl<O: Clone> IntoFuture for Observation<O> {
    type Output = O;
    type IntoFuture = ObservationFuture<O>;

    fn into_future(self) -> Self::IntoFuture {
        ObservationFuture {
            observation: self,
            wakers: Vec::new(),
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests;
#[cfg(all(test, not(loom)))]
mod tests;
