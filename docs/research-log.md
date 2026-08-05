# Research log

Record hypothesis, primary sources, implementation, Loom/Miri results,
benchmark distribution, allocations, retained memory, and decision for every
experiment. The initial `Mutex<HashMap>` plus per-subject condition variable is
a correctness baseline, not a preferred design.

## Final state (TL;DR)

The complete design is green, stable, and independently reviewed:

- **Design**: `parking_lot::Mutex<Entries { map: SmallMap<K, SlotEntry>,
  pool: Vec<Arc<Slot>> }>` key table (SmallMap = inline Vec <= 4 entries,
  Eq-only scan, promoting to a fixed-seed FxHash HashMap) + `AtomicUsize`
  generation counter + lock-free slot (single-word state `COMPLETED |
  HAS_WAITER`, `UnsafeCell<Option<O>>` outcome, waiters under their own
  mutex as `Thread | Waker`) + recycled-slot pool (retire-time
  `strong_count == 1` proof, cap 128, stale-waiter drain at reset).
- **Throughput**: ~53.2M observations/s stable on AC (three runs
  53.5/53.1/53.3M; recorded best 54.5M), +581% over the 7.8M baseline; zero
  allocations per op; 112 B retained per subject+observer; hot path p50/p99
  41/42 ns; wait round-trip p50 ~3 us.
- **API**: `try_get` (lock-free), `wait`, `wait_timeout`, `register_waker`
  (std `Waker`, async adapter hook), `into_outcome` (move-only), thiserror
  errors; runnable adapter example in `examples/actorpass_adapter.rs`.
- **Verification**: 11 in-lib loom models of the real implementation
  (preemptions 3 and 7), Miri 12/12 on real-thread tests, full frozen gate
  green, independent concurrency review: design sound, findings addressed.
- The unsafe surface (the outcome cell + pool reuse) is documented below
  with its invariants; any change to the state protocol must re-run loom
  and Miri.

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

## EXPERIMENT 11 - API contract completion (perf-neutral, kept)

The prompt requires "typed and potentially move-only outcomes" and "an
asynchronous actorpass adapter without runtime type erasure". The frozen
`try_get`/`wait` need `O: Clone`; added two methods that complete the
contract without changing the hot path:
- `Observation::into_outcome(self) -> Option<O>`: moves the outcome by
  value when this handle is the last slot reference (`Arc::try_unwrap` +
  `UnsafeCell::into_inner`, exclusive by move). Supports non-`Clone`
  outcomes; returns `None` while pending or shared.
- `Observation::register_waker(&Waker) -> bool`: the std `Waker` hook for
  async adapters, using the exact `HAS_WAITER` protocol as `wait` (SeqCst
  RMW, recheck under the waiters lock, drain wakes). Waiters became
  `enum Waiter { Thread, Waker }`; the drain match is off the hot path
  (only runs when a waiter registered).
Primary unchanged (49.6M). std 9/9, loom 10/10 (into_outcome racing
complete never torn), Miri 9/9, gate green.

## Independent concurrency review (fresh-eyes, all findings addressed)

A specialist reviewer audited the final design against the documented
invariants. Verdict: the concurrency design is sound (overall confidence
0.85) - every outcome-cell read is gated behind Acquire+ observation of
COMPLETED, the single-word RMW total order closes both lost-wakeup
interleavings, the pool recycle proof holds, and the SmallMap/promotion,
drain-outside-lock, checked deadline arithmetic, and Relaxed generation
counter are correct. Findings addressed:
1. wait/wait_timeout deregister the current thread on early-return and
   timeout paths and dedupe registrations across loop iterations - stale
   Thread registrations cannot survive into pooled slots.
2. Slot::reset drains the waiters Vec when HAS_WAITER is set (covers a
   waker whose caller dropped its observation before completion) - bounded
   retention is now exact (no dead waiter payloads in the pool, no
   cross-generation spurious fires).
3. wait_timeout Panics doc lists both panic sources (protocol violation,
   Instant overflow) and the typo is fixed.
4. register_waker_racing_complete_never_loses_outcome stress test (100x
   real-thread race; the waker path is not loom-modelable - std Waker is
   not a loom type).
Acknowledged unfixable: tests/loom.rs is frozen (the gate diffs it) and
models none of the crate; the real loom models live in src/loom_tests.rs.
All fixes perf-neutral (3 direct runs 50.41-50.52M, identical to the
pre-fix stable value). std 12/12, loom 11/11 at preemptions 3 and 7, Miri
12/12, gate green.

## arXiv literature check (recorded honestly)

The arXiv API (export.arxiv.org) was queried for the mechanism's topics:
"eventcount"/"event count" (no results - the eventcount literature lives on
Vyukov's 1024cores site, already recorded), async-Rust futures/cancellation
(closest hits: "Deadlock-free asynchronous message reordering in Rust with
multiparty session types", "Provably Fair Cooperative Scheduling" - both
tangential), and wakeup/notification primitives (no results). Conclusion:
the specific primitive (a keyed completion-observation cell with thread +
waker registration, dedup, and drop-based cancellation) has no direct arXiv
literature; its foundations are the primary sources already recorded here
(Vyukov eventcounts; std `park`/`unpark` token semantics; std `Waker`
source - `will_wake`/`clone_from`; tokio `AtomicWaker`; the async-Rust
cancellation analysis). Allocation audit of the per-op path (subject +
observe + complete + try_get + retire): zero heap allocations - the pool
eliminated the slot alloc, the inline SmallMap avoids map reallocation, the
dedup eliminated repeated waker pushes; the only per-subject allocation for
heap keys is the `key.clone()` into the map entry (inherent: both the entry
and the Subject own the key; free for the `u64` keys of the frozen
workload).

## EXPERIMENT 16 - waiters AtomicPtr, drop the Arc indirection (kept, measured)

Hypothesis (memory-efficiency axis): EXP14's `OnceLock<Arc<Mutex<Vec<Waiter>>>>`
(16B inline, 40B heap) still carries an unnecessary `Arc`: the registry mutex
is owned by the slot itself, which outlives every waiter, so a shared
ownership token is pointless. Replaced with `AtomicPtr<Mutex<Vec<Waiter>>>`
(8B inline, 32B heap): null until the first waiter; the first access
`Box::into_raw`s a fresh `Mutex<Vec<Waiter>>` and CAS-publishes it (a loser
of the init race reclaims its own box and uses the winner's); the slot's
final `Drop` reclaims the box (`Box::from_raw`). Slot 32->24B (48->40B with
the Arc header); retained 88->80B (-9%); alloc stays 0/op; primary 54.3M
(within the AC range). The field remains entirely off the hot path.

New unsafe invariant (replaces EXPERIMENT 14's OnceLock clause): the waiters
pointer, when non-null, points to a `Box<Mutex<Vec<Waiter>>>` owned by the
slot; it is published by the init CAS (Release) and reclaimed only by the
slot's final drop (the last Arc reference), so every access to it happens
while the slot is live (an Acquire load returns either null or a live
pointer). Losing the init race reclaims the loser's box, which was never
published.

Verification: std 15/15, loom 11/11 at preemptions 3 and 7 (the multi-waiter
models exercise the CAS-init race), Miri 15/15 (including the `Box::from_raw`
reclaim under the real-thread model), clippy clean, gate CHECK OK.

## EXPERIMENT 14 - lazy waiters (kept, measured)

Hypothesis (memory-efficiency axis): every slot carries a 32B inline
`Mutex<Vec<Waiter>>` even when no waiter ever registers, and the hot path
never touches the waiters field. Replaced with `OnceLock<Arc<Mutex<Vec<Waiter>>>>`
(16B inline; 40B heap only on the first waiter registration). All wait-path
call sites route through a `get_or_init` accessor; complete/reset drains
check `get()` (always `Some` when `HAS_WAITER` is set). Measured: retained
112->96B per subject+observer (-14%); alloc stays 0/op; primary perf-neutral
(54.4M, within the AC range). Pure memory win: the field is off the hot path
entirely.

## EXPERIMENT 15 - outcome cell, MaybeUninit + OUTCOME_VALID bit (kept, measured)

Hypothesis (memory-efficiency axis): the outcome cell `UnsafeCell<Option<O>>`
(16B for the u64 harness) carries a redundant Option tag - `COMPLETED`
already gates every read, and `into_outcome` takes the value only at the
last reference (`Arc::try_unwrap`), so a "taken" state is never observed by
another reader. The tag moved into the state word as an `OUTCOME_VALID` bit
(1<<2), set by complete's RMW (`fetch_or(COMPLETED | OUTCOME_VALID, Release)`)
and cleared by reset's recycle and into_outcome's take. Readers keep the
single `state & COMPLETED` AND-compare (identical instruction count). The
cell is `UnsafeCell<MaybeUninit<O>>` (8B): written once by complete
(`MaybeUninit::write`, happens-before the Release RMW), read only after
COMPLETED is observed (Acquire), dropped exactly once (reset's
`assume_init_drop` under the entries write lock, or into_outcome's
`assume_init_read` + bit clear, or the slot's manual `Drop` gated on the
bit - the pool's retired slots and taken cells have the bit clear, so the
final drop is a no-op there).

New unsafe invariants (replace EXPERIMENT 6's "Option cell"):
1. The outcome cell is written exactly once, by complete, before the
   `COMPLETED|OUTCOME_VALID` Release `fetch_or`.
2. The outcome cell is read only after observing `COMPLETED` via an
   Acquire-or-stronger load of `state`, which synchronizes-with that
   Release RMW.
3. The outcome cell is dropped exactly once: reset (pooled slot, no
   observers, under the entries write lock; the subsequent Release store of
   `state` publishes the drop), into_outcome (last reference via
   `Arc::try_unwrap`, `assume_init_read` then `fetch_and(!OUTCOME_VALID)`),
   or the slot's final `Drop` (last Arc reference; gated on `OUTCOME_VALID`).
   `OUTCOME_VALID` is the single source of truth for whether the cell holds
   a live value.

Measured: slot 56->48B (72->64B with the Arc header); retained 96->88B
(-8%); alloc stays 0/op; primary 52.3M (within the AC range); contention
slightly improves (8t 11.4M vs 10.8M, 16t 13.0M vs 12.6M) - the smaller
slot packs better into cache lines. Verification: std 15/15, loom 11/11 at
preemptions 3 and 7 (models exercise the outcome read, into_outcome racing
complete, and the wait path), Miri 15/15 (including the into_outcome
move-out under the real-thread model), clippy clean, gate CHECK OK.

## EXPERIMENT 13 - entries RwLock (kept, measured)

Hypothesis (correcting an earlier analytical dismissal): the observe read (1
of 3 entries-lock acquisitions per op) parallelizes under an `RwLock`.
Measured: primary neutral (52.9M vs 53.2M Mutex over 3 direct runs each),
contention improves +10-20% (2t 26.6->32.1M, 4t 19.7->21.8M, 8t 9.6->10.5M,
16t 11.8->12.2M, 1t 52.7->54.9M). The read-parallelization gain outweighs
the marginally higher write-lock cost. Lesson: "writes dominate" was wrong
at the handoff-latency level - the reads' parallel acquisitions matter.
observe takes a read lock; subject/retire take write locks; the pool stays
inside Entries under the write lock; loom uses `loom::sync::RwLock`.

## Waker-registration review pass (all feedback points addressed)

External review feedback on the async registration path, addressed with
primary-source research:

1. **`register_waker` accumulated duplicate/abandoned wakers** (each call
   pushed a fresh clone; dropping the Observation did not deregister). Fixed
   with the std-endorsed pattern: a registration whose `Waker::will_wake` an
   already-registered waker is idempotent (no clone, no allocation on the
   repeated path). `will_wake` is two pointer comparisons (`std` wake.rs);
   `Waker::clone_from` exists precisely for this dedup.
2. **No cancellation hook for async observers.** Added `IntoFuture for
   Observation` producing `ObservationFuture`, whose poll registers the
   task's waker idempotently and whose `Drop` deregisters it (the canonical
   cancellation mechanism: dropping a future cancels it; see sunshowers,
   "Cancelling async Rust"). An observation is now directly awaitable.
   Replaces the tokio `AtomicWaker` "overwrite existing waker" pattern
   (tokio src/sync/task/atomic_waker.rs) for the multi-observer fanout case.
3. **Stale doc**: `wait_timeout` still claimed timed-out waiters stay
   registered; the implementation (review-fix pass) deregisters them.
   Corrected.
4. **Loom build warning**: unused `deadline` under `--cfg loom`. The deadline
   computation and the `Instant` import are now cfg-gated; the loom build is
   warning-free.

Notable discovery while testing: under Miri, `Waker::will_wake` between an
original and its clone is spuriously false for `Waker::from(Arc)`-derived
wakers - std's `Wake` vtable is a const-promoted temporary
(`&RawWakerVTable::new(...)`) created at each call site; native builds merge
the identical constants (same address), Miri keeps them distinct. The
dedup falls back safely (a duplicate entry) when `will_wake` is
conservatively false. Tests use a single-static-vtable raw waker so the
dedup is exact under both native and Miri (the test's own unsafe is
Miri-checked by the run).

Verification: std 15/15 (3 new tests: idempotent registration, future
resolves, future-drop deregisters), loom 11/11 at preemptions 3 and 7,
Miri 15/15, clippy clean, gate green, example extended with the future
flow.

## Quantified rejection: hazard-pointer lock-free observe

The last remaining perf lever (removing the observe entries lock) was
analyzed at the cost level: a hazard-pointer read protocol (atomic load of
the slot pointer, hazard publish, re-validate, `increment_strong_count`,
unpublish) costs ~5-7 ns per read, versus ~5 ns for the lock it replaces
(lock + inline scan). For the single-observer actorpass pattern the hazard
protocol is not a win, and it would add a second unsafe component with
reclamation proof obligations. Rejected on cost, not just complexity.

## Compliance fix (perf-neutral): thiserror error types

`SubjectExists`/`UnknownSubject` now derive `thiserror::Error` with Display
messages (global devrandom rule: all error types use thiserror). Added the
workspace dep; no behavior change (off the hot path). std 11/11, loom
11/11, Miri 11/11, gate green. Primary 53.6M (new session best; run
variance landed favorably).

## EXPERIMENT 12 - wait_timeout (API addition, perf-neutral, kept)

`Observation::wait_timeout(Duration) -> Option<O>`: bounded blocking for
adapters that must not block forever on a never-completed generation (e.g.,
a crashed child). Same HAS_WAITER registration protocol as `wait`;
`park_timeout` under the deadline, with the state re-check before the
deadline test so a racing completion is still observed; a timed-out waiter
stays registered (a later completion only produces a spurious wake). The
loom build parks without a clock (`park_timeout` is not modeled; the
registration protocol is covered). std 11/11, loom 11/11, Miri 11/11, gate
green. Primary 50.5M (perf-neutral).

## Adapter example (deliverable, perf-neutral)

`crates/observepass/examples/actorpass_adapter.rs` is executable proof of the
integration contract: one generation per `Subject` at a key,
`register_waker` before task completion (std `Waker`, no Tokio; the returned
flag closes the registration-races-completion race), `complete` exactly once,
an executor loop reading via `try_get`, peer observation after completion,
and the adapter's `ChildStopped`/`PeerStopped` translation in its own
vocabulary (observepass itself knows none of it). Runs clean, clippy 0, gate
green. Primary unchanged (49.9M).

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
| 13 | EXP11: into_outcome (move-only) + register_waker (async hook) | 49,630,870 | +535% | keep (perf-neutral contract completion) |
| 14 | EXP12: wait_timeout(Duration) -> Option<O> (bounded blocking) | 50,468,410 | +546% | keep (perf-neutral API addition) |
| 15 | adapter example (deliverable, no lib change) | 49,937,473 | +539% | keep |
| 16 | EXP13: thiserror error types (compliance, perf-neutral) | 53,624,694 | +586% | keep |
| 17 | const-capacity promotion (compliance, perf-neutral) | 54,523,894 | +598% | keep |
| 18 | review fixes (stale-waiter deregistration + reset drain, perf-neutral) | 50,470,853 | +546% | keep |
| 19 | AC re-measurement (machine returned to AC; definitive record) | 52,450,302 | +571% | keep |
| 20 | EXP13: entries Mutex -> RwLock (observe reads parallelize) | 53,591,763 | +586% | keep (primary-neutral; contention +10-20%) |

Final design: single `Mutex<Entries { map: SmallMap<K, SlotEntry>, pool:
Vec<Arc<Slot>> }>` key table (parking_lot; SmallMap = inline Vec <= 4
entries promoting to FxHash HashMap) + generation counter `AtomicUsize` +
lock-free slot (single-word state: COMPLETED | HAS_WAITER;
`UnsafeCell<Option<O>>`; waiters under their own `parking_lot::Mutex<Vec<Waiter>>`,
`Waiter = Thread | Waker`) + recycled-slot pool (retire-time strong-count
proof, cap 128, stale-waiter drain at reset). API also provides
`wait_timeout`, `register_waker`, and `into_outcome`; errors via thiserror.

Final numbers (run #26, definitive AC record): primary 52.5M recorded,
three direct runs 53.5/53.1/53.3M (stable ~53.2M, best 54.5M at run #18);
seq_complete_first 46.2M, retire_recreate 70.2M, cancel 53.8M;
fanout(8) 153.2M observations/s; hot path p50/p99 41/42 ns; wait round-trip
p50/p99 3.0/6.7 us; contention 1t/2t/4t/8t/16t 52.8M/31.8M/22.2M/10.7M/
12.2M; alloc 0 B / 0 blocks per op (pooled); retained 112 B / 1 block per
subject+observer; after-drop residue 1088 B (no leak). The frozen criterion
bench measures complete_then_observe at 18.8 ns/iteration on AC (27.0 ns on
battery - a 1.44x ratio confirming the power-governor depression).

## Actorpass integration contract

- Map one actor generation to a `Subject` at a key; publish the outcome via
  `Subject::complete` exactly once on task completion; translate
  observations into pure `ChildStopped`/`PeerStopped` events.
- `Observation::try_get` is lock-free (single Acquire load + clone): safe on
  any thread. `Observation::wait` parks the calling thread and wakes on
  completion (p50 2.5 us on macOS). Async adapters call
  `Observation::register_waker(&Waker)` (std `Waker`, no Tokio) - same
  HAS_WAITER protocol; returns true when already published; wakers may fire
  spuriously, callers re-read. Move-only outcomes use
  `Observation::into_outcome` (moves out when the handle is the last slot
  reference). No actor vocabulary, no runtime type erasure: the API is
  generic over `K`/`O`; `O` needs `Clone` for `try_get`/`wait` and
  `Send + Sync` for the observation to be shareable across threads (outcome
  becomes immutable at publication).
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
- RwLock entries map: initially dismissed analytically (writes are 2/3 of
  map ops, so reader parallelism is capped); MEASURED anyway in EXP13 and
  KEPT - the observe read (1 of 3 acquisitions per op) parallelizes under an
  RwLock and contention improves +10-20% with a neutral primary. The lesson
  (handoff latency, not instruction count, dominates under contention) drove
  the EXP14/EXP15 re-audits of analytically-dismissed memory items.
- dhat for allocation accounting: 0.3 dropped the programmatic stats API;
  hand-rolled counting GlobalAlloc used instead (documented invariants).

## Remaining risks

- The unsafe slot's correctness rests on the documented invariants; loom
  models the real implementation (11 tests, preemptions 3 and 7) and Miri
  passes 12 real-thread tests. Any future change to the state protocol must
  re-run both.
- `wait()` (without a timeout) on a subject that is never completed blocks
  forever; `wait_timeout` bounds the blocking, and `register_waker` is the
  non-blocking alternative.
- park/unpark wait is per-thread token-based; a thread waiting on two
  observations concurrently can consume a cross-wake token as a spurious
  wakeup (harmless - the wait loop re-checks). Registrations are deduped
  and deregistered on early return/timeout, so the waiters Vec stays small.
- The primary metric is sensitive to OS scheduling (E-core vs P-core for
  the short frozen process) and to the power source: on battery, macOS caps
  the boost clocks and every scenario measures uniformly ~30% lower
  (~35.5M for the primary; confirmed `pmset -g batt` = discharging). The
  battery window (runs #21-25, all flagged) was a pure power-governor
  artifact - the code never changed. The definitive AC re-measurement
  (run #26) confirms full recovery: stable ~53.2M (three runs 53.5/53.1/53.3M),
  best 54.5M. Relative comparisons within a power state are valid.

