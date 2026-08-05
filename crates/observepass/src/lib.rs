//! Generation-safe completion publication and observation.

use core::hash::Hash;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

fn recover<T>(error: PoisonError<T>) -> T {
    error.into_inner()
}

struct Slot<O> {
    outcome: Mutex<Option<O>>,
    completed: Condvar,
}

struct Entry<O> {
    generation: u64,
    slot: Arc<Slot<O>>,
}

struct Inner<K, O> {
    next_generation: Mutex<u64>,
    entries: Mutex<HashMap<K, Entry<O>>>,
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
                next_generation: Mutex::new(1),
                entries: Mutex::new(HashMap::new()),
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
    /// Panics if the process exhausts all `u64` generations.
    pub fn subject(&self, key: K) -> Result<Subject<K, O>, SubjectExists<K>> {
        let mut entries = self.inner.entries.lock().unwrap_or_else(recover);
        if entries.contains_key(&key) {
            return Err(SubjectExists(key));
        }
        let mut next = self.inner.next_generation.lock().unwrap_or_else(recover);
        let generation = *next;
        *next = next
            .checked_add(1)
            .expect("observation generation exhausted");
        let slot = Arc::new(Slot {
            outcome: Mutex::new(None),
            completed: Condvar::new(),
        });
        entries.insert(
            key.clone(),
            Entry {
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
        let entries = self.inner.entries.lock().unwrap_or_else(recover);
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
    generation: u64,
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
        *self.slot.outcome.lock().unwrap_or_else(recover) = Some(outcome);
        self.completed = true;
        self.slot.completed.notify_all();
    }
}

impl<K, O> Drop for Subject<K, O>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        let mut entries = self.inner.entries.lock().unwrap_or_else(recover);
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
        self.slot.outcome.lock().unwrap_or_else(recover).clone()
    }

    /// Block the current thread until completion.
    pub fn wait(&self) -> O {
        let mut outcome = self.slot.outcome.lock().unwrap_or_else(recover);
        loop {
            if let Some(outcome) = outcome.clone() {
                return outcome;
            }
            outcome = self.slot.completed.wait(outcome).unwrap_or_else(recover);
        }
    }
}
