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

