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
- Result: kept. See the final table below.

## EXPERIMENT 8/9 - recycled-slot pool (safe)

- Hypothesis: the 72 B `Arc<Slot>` malloc per op is ~1/3 of the hot path and
  a shared-allocator contention point at 8 threads. Slots whose retire finds
  no live observers can be recycled.
- Key insight: at retire, while holding the entries lock and after removing
  the entry, no new observer can reference the slot (observe needs both the
  lock and the entry), so the strong count can only decrease: exactly 1
  (the subject's own handle) iff no observer holds it. `Arc::strong_count`
  read under the lock is therefore a sound "no observers" test.
- EXP8 (superseded): pool behind its own nested `Mutex` - primary flat
  (the 2 uncontended nested locks ate the malloc gain).
- EXP9 (kept): pool folded into `Entries { map, pool }` behind the single
  entries mutex - `&mut` through the guard, zero extra locks. On reuse the
  slot is reset (outcome cleared + `state.store(0, Release)`); stale
  COMPLETED/HAS_WAITER bits cannot leak (loom test
  `pooled_slot_reuse_isolates_generations`). Cap `SLOT_POOL_CAP = 128`
  bounds retained memory. Auto-trait note: the space now requires
  `O: Send + Sync` (the pooled `Arc<Slot<O>>` must be `Send`).
- Result: primary +2.9% (27.9M -> 28.7M); seq scenarios ~44M; contention
  8t 8.5M -> 11.4M; alloc 0 blocks / 0 bytes per op; retained unchanged
  (112 B); loom 8/8, Miri 5/5.

## EXPERIMENT 10 - SmallMap hybrid key storage (kept)

- Observation: with the pool (EXP9) the op was ~20-23 ns and the key table's
  hash+probe work was a large, avoidable share for the transient actorpass
  case (~1 live entry): `HashMap` still hashes and probes on every
  subject/observe/retire even at size 1.
- Hypothesis: an inline `Vec<(K, SlotEntry)>` (Eq-only linear scan, no
  hashing) for the first `INLINE_CAP = 4` live entries, promoting to the
  FxHash `HashMap` beyond that, preserves O(1) retention-scale behavior
  while cutting the transient map cost to a 1-2 element scan.
- Promotion is one-time (never demotes); the inline Vec's capacity is stable
  under the transient workload, so the 0-allocations-per-op property is
  preserved. Semantics unchanged (vacancy check, generation-checked retire,
  stale-retire restore).
- Result: primary 28.7M -> 49.7M (+73%; A/B verified over 3 runs each:
  48.8-50.3M vs 26.2-28.1M). retire_recreate 70.1M; contention 4t 19.5 ->
  24.5M, 8t 11.4 -> 12.0M; alloc 0/op; retained 112B. Loom 9/9 (added
  `small_map_promotion_preserves_semantics`), Miri 5/5, gate green.

## Rejected: per-space observe cache

An `AtomicU64`-keyed cache of the last observed slot could skip the observe
entries lock, but a cached slot can outlive its generation under rapid
address reuse (retire+resubject between cache fill and hit), handing an
observer a retired generation - a generations-crossing hazard that the map
under the lock cannot produce. Invalidation under the entries lock cannot
close the race (observe reads the cache outside the lock). Rejected; the
entries lock stays on the observe path.

## Experiment results (frozen workload, best-of-5, observations_per_second)

| # | change | primary | vs baseline | decision |
|---|--------|---------|-------------|----------|
| 1 | baseline `Mutex<HashMap>` + Condvar slot | 7,815,698 | — | baseline |
| 2 | EXP1: atomic generation (AtomicUsize fetch_add) + single `Entry` lookup | 11,492,376 | +47% | keep |
| 3 | EXP2: eventcount slot (Condvar -> park/unpark + waiters Vec) | 14,014,443 | +79% | keep |
| 4 | EXP3a: internal map hasher SipHash -> fixed-seed FxHash | 13,769,008 | +76% | keep (noise-neutral; map-heavy scenarios +16..+34%) |
| 5 | EXP4: std::sync::Mutex -> parking_lot (std Mutex lazily mallocs its pthread mutex, 64B, on first lock - backtraced) | 25,889,716 | +231% | keep |
| 6 | EXP5: 64-shard key table (FxHash shard selection) | 18,840,611 | +141% | REJECTED (-27%: shard hash + indirection cost more than contention gained) |
| 7 | EXP6: lock-free outcome slot (unsafe: state word + UnsafeCell, waiters mutex) | 27,897,181 | +257% | keep |
| 8 | EXP7: retire via remove-then-restore (single lookup) | 26,160,291 | +235% | REJECTED (-6%: hashbrown value-returning remove slower than get+conditional remove) |
| 9 | harness: wait_roundtrip p50/p99 scenario | 27,932,473 | +257% | keep (no lib change) |
| 10 | EXP8: slot pool, nested pool Mutex | 27,702,717 | +254% | superseded by EXP9 (folded pool, no nested locks) |
| 11 | EXP9: pool folded into Entries{map,pool} behind one mutex | 28,720,880 | +267% | keep |
| 12 | EXP10: SmallMap hybrid (inline Vec <=4, promotes to HashMap) | 49,685,943 | +536% | keep |

Final design: single `Mutex<Entries { map: SmallMap<K, SlotEntry>, pool:
Vec<Arc<Slot>> }>` key table (parking_lot; SmallMap = inline Vec <= 4
entries promoting to FxHash HashMap) + generation counter `AtomicUsize` +
lock-free slot (single-word state: COMPLETED | HAS_WAITER;
`UnsafeCell<Option<O>>`; waiters under their own `parking_lot::Mutex<Vec<Thread>>`)
+ recycled-slot pool (retire-time strong-count proof, cap 128).

Final numbers (run #12): primary 49.69M; seq_observe_first 51.4M,
seq_complete_first 49.6M, retire_recreate 70.1M, cancel 51.7M;
fanout(8) 149.3M observations/s; hot path p50/p99 41/42 ns; wait round-trip
p50/p99 2.3/6.1 us; contention 1t/4t/8t 51.6M/24.5M/12.0M; alloc 0 B / 0
blocks per op (pooled); retained 112 B / 1 block per subject+observer;
after-drop residue 1088 B (no leak).

## Actorpass integration contract

- Map one actor generation to a `Subject` at a key; publish the outcome via
  `Subject::complete` exactly once on task completion; translate
  observations into pure `ChildStopped`/`PeerStopped` events.
- `Observation::try_get` is lock-free (single Acquire load + clone): safe on
  any thread. `Observation::wait` parks the calling thread and wakes on
  completion (p50 2.7 us on macOS); for async use, the slot's
  HAS_WAITER/waiter-registry protocol extends to a waker slot: register a
  `std::task::Waker` instead of a `Thread`, wake it from the drain. No Tokio,
  no actor vocabulary, no runtime type erasure: the API is generic over
  `K`/`O`; `O` needs `Clone` for `try_get`/`wait` and `Send + Sync` for the
  observation to be shareable across threads (outcome becomes immutable at
  publication).
- Cancellation: dropping an `Observation` never touches the slot; it cannot
  obstruct completion.
- Retention: bounded by explicit ownership (Subject retains the key entry;
  observers retain the slot Arc); all memory is freed when the last handle
  drops (after-drop residue ~1 KB = stdout buffer + runtime).

## Rejected designs (with evidence)

- RCU / epoch reclamation / hazard pointers for the slot: not needed - `Arc`
  already provides safe reclamation with bounded cost; observers hold the
  slot past retirement. Would only matter if removing the one 72 B Arc
  allocation per subject became the dominant cost (it is ~1/3 of the op now).
- Sharded key table (EXP5): measured -27% (see table).
- remove-then-restore retire (EXP7): measured -6% (see table).
- RwLock entries map: not tried; writes are 2/3 of map ops, so reader
  parallelism is capped; uncontended parking_lot RwLock reads cost about the
  same as its Mutex. Low expected value.
- dhat for allocation accounting: 0.3 dropped the programmatic stats API;
  hand-rolled counting GlobalAlloc used instead (documented invariants).

## Remaining risks

- The unsafe slot's correctness rests on the documented invariants; loom
  models the real implementation (7 tests, preemptions 3 and 7) and Miri
  passes the real-thread tests. Any future change to the state protocol must
  re-run both.
- `wait()` on a subject that is never completed blocks forever (same as the
  baseline Condvar design); no timeout API exists.
- park/unpark wait is per-thread token-based; a thread waiting on two
  observations concurrently can consume a cross-wake token as a spurious
  wakeup (harmless - the wait loop re-checks).
- The primary metric is sensitive to CPU frequency state (measured
  +/-3.5% run-to-run); best-of-5 and in-script ordering mitigate it.

