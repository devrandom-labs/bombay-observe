# Research log

Record hypothesis, primary sources, implementation, Loom/Miri results,
benchmark distribution, allocations, retained memory, and decision for every
experiment. The initial `Mutex<HashMap>` plus per-subject condition variable is
a correctness baseline, not a preferred design.

## Primary sources consulted

- **Vyukov, "Eventcounts"** (lock-free condition variables; pioneered with
  Chris Thomasson): waiter `prepare_wait` captures an epoch key, publisher
  bumps the epoch and wakes sleepers; check-then-sleep race closed by the
  atomic epoch. https://www.1024cores.net/home/lock-free-algorithms/eventcounts
  Production ports: folly `EventCount`
  (https://github.com/facebook/folly/blob/main/folly/experimental/EventCount.h),
  Eigen `EventCount.h` / Taskflow `notifier.hpp` (Vyukov original).
- **Rust std `thread::park`/`Thread::unpark` token semantics** (primary):
  `unpark` atomically makes the token available if absent; "unpark followed by
  park will result in the second call returning immediately"; park may return
  spuriously without consuming the token; no unknown code (e.g. `println!`)
  between registration and `park`. `unpark` is Release, `park` is Acquire.
  https://doc.rust-lang.org/std/thread/fn.park.html
- **tokio `oneshot` internals** (production single-completion primitive):
  `Inner { state: AtomicUsize, value: UnsafeCell<Option<T>>, rx_task: single
  waker slot }`; sender writes the value while the VALUE_SENT bit is unset
  (receiver cannot read yet), then CAS-completes and wakes the registered
  waker. The async-grade design for observepass's adapter.
  https://github.com/tokio-rs/tokio/blob/master/tokio/src/sync/oneshot.rs
- **Rejected now, revisit only if safe designs prove insufficient**: RCU /
  epoch reclamation / hazard pointers — needed only when observers hold raw
  pointers past retirement; `Arc` already provides safe reclamation. Explicit
  futex syscalls — not exposed by stable std (std Mutex/Condvar use futexes
  internally on Linux); `park`/`unpark` is the portable stable primitive.

## EXPERIMENT 1 — atomic generation + single-entry lookup (map side)

- Hypothesis: `subject()` takes two mutexes (entries + next_generation) and
  two map lookups (contains_key + insert) per registration. Replacing the
  generation mutex with an `AtomicUsize` `fetch_add` and collapsing the
  lookups into one `Entry` API call removes one lock acquisition and one
  lookup from every registration, improving the frozen workload.
- Design: `Inner.next_generation: AtomicUsize` (Relaxed — RMW total order
  gives distinct values; entry visibility governed by the entries mutex);
  `Entry::Vacant/Occupied` single lookup; generation width `usize` (private).
- Loom: real implementation gains `#[cfg(loom)]` swaps to
  `loom::sync`/`loom::collections::HashMap` plus in-lib loom tests
  (subject/observe/retire races).
- Result: see benchmark table below.

## EXPERIMENT 6 — lock-free outcome slot (unsafe, tokio-oneshot derived)

Context: with parking_lot (EXP4), per-op cost is dominated by the two slot
locks (`complete`, `try_get`). EXP5 (64-shard table) regressed and was
discarded. Design derives from tokio `oneshot` (`AtomicUsize` state +
`UnsafeCell` value + single waiter slot) extended with a waiters Vec.

Slot layout:

```text
state:   AtomicUsize   bit0 = COMPLETED, bit1 = HAS_WAITER (one word, RMW-total-ordered)
outcome: UnsafeCell<Option<O>>
waiters: Mutex<Vec<Thread>>   (registration + drain only)
```

Protocol:
- complete: write outcome; `fetch_or(COMPLETED, Release)`; if the RMW read
  HAS_WAITER, drain waiters under the mutex and unpark all outside it.
- try_get: `state.load(Acquire)`; if COMPLETED, clone the outcome.
- wait: load state; if COMPLETED return; `fetch_or(HAS_WAITER, SeqCst)`;
  lock waiters, push, reload state (SeqCst) — if COMPLETED return; unlock;
  park; repeat.

### Unsafe invariants (proof obligations)
1. The outcome cell is written exactly once (Subject::complete is
   single-call; `completed` flag asserts) and the write happens-before the
   COMPLETED bit is set (program order + the Release RMW).
2. The outcome cell is read only after observing COMPLETED via an
   Acquire-or-stronger load of `state` (try_get, wait's pre-park and
   post-push rechecks), which synchronizes-with the Release RMW. No reader
   can observe the bit before the write, so no torn/empty reads.
3. No lost wakeup: waiter's `fetch_or(HAS_WAITER, SeqCst)` and complete's
   `fetch_or(COMPLETED, Release)` are RMWs on one location, totally ordered
   by the modification order. If complete's RMW runs first and reads no
   HAS_WAITER, the waiter's later SeqCst state load (after its own RMW in
   program order) reads COMPLETED by coherence and returns without parking.
   If the waiter's RMW runs first, complete sees HAS_WAITER and drains; the
   waiters mutex serializes drain vs push, and park consumes the token
   (std token semantics: unpark-before-park returns immediately).
4. Reclamation: no raw pointer outlives the Arc<Slot>; `UnsafeCell` is
   dropped with the Arc. No epoch/hazard-pointer burden.
5. Cancellation: dropping an Observation never touches the slot; a parked
   waiter's stale registration only produces a spurious token.
- Loom models the real implementation (`loom::cell::UnsafeCell`,
  `loom::sync::atomic`, `loom::sync::Mutex`, `loom::thread::{park,current}`),
  including the wait path. Miri runs the real-thread unit tests.
- Expected: `try_get` lock-free and `complete` lock-free when no waiter has
  ever registered; slot grows by the separate waiters mutex (retained +~8B).

