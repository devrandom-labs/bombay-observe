# Autoresearch observepass for actorpass

Discover the fastest, most memory-efficient, race-proof completion-observation
mechanism that plugs into actorpass without type erasure. Read primary research
on futures/promises, eventcounts, wait queues, RCU, epochs, hazard pointers,
generation counters, completion ports, and production task/join mechanisms.
Search for newer work too and record sources and failed ideas in
`docs/research-log.md`.

The actorpass workload is many short-lived subjects, mostly one parent observer,
occasional peer fanout, completion racing registration, and typed move-only
outcomes. Required semantics: observe-before-complete and complete-before-observe
both succeed; no loss or duplicate publication; generations never cross;
retirement is stale-safe; cancelled observers do not block completion; retention
and allocation are bounded and measured. The final API must permit an async
actorpass adapter without requiring Tokio or actor vocabulary in observepass.

State one falsifiable hypothesis per experiment. Never alter frozen tests,
benchmarks, perf harnesses, or gates. Unsafe work requires written invariants,
Loom coverage of the real implementation, and Miri. Compare throughput, p50/p99,
fanout, allocations, retained bytes, and contention—not one lucky score.

## Required optimization cycle

For each iteration:

1. Record one falsifiable hypothesis and the expected affected metric.
2. Run `.auto/checks.sh` before measuring. A correctness failure rejects the
   candidate regardless of speed.
3. Run `.auto/measure.sh` and retain the candidate only when repeated results
   improve the declared metric without a material regression in the remaining
   workload matrix.
4. Record the patch, raw measurements, environment, variance, and rejection or
   acceptance rationale in `docs/research-log.md`.
5. Re-run the full gate after every accepted candidate. For unsafe or
   synchronization changes, also run the deep Loom and Miri commands below.

Do not optimize against `SCORE` alone. Check primary throughput, p50 and p99,
1/2/4/8/16-thread contention, fanout, retire/recreate, allocation count and
bytes, and retained bytes per subject plus observer. Report medians and ranges
from repeated runs; reject unexplained high variance and improvements within
noise.

## Frozen correctness oracle

Every candidate must preserve executable checks for:

- observe-before-complete and complete-before-observe;
- exactly-once publication and fully initialized outcome visibility;
- concurrent same-key subject creation has exactly one winner;
- stale retirement cannot remove a replacement generation;
- pooled slot reuse cannot leak completion state or outcomes across generations;
- all registered waiters wake, while late waiters return without parking;
- timeout success, timeout expiry, zero timeout, and completion at the timeout boundary;
- cancelled or dropped waiters do not block completion or corrupt the wait list;
- move-only `into_outcome` behavior, including its completion race;
- outcome destruction exactly once on final release, pool reset, and ownership transfer;
- inline-map promotion preserves lookup and retirement semantics.

`.auto/checks.sh` freezes both `src/tests.rs` and `src/loom_tests.rs`; adding a
test during research is allowed only as a separate reviewed oracle update made
against the baseline before resuming optimization. Never weaken, delete,
ignore, rename around, or conditionally bypass a frozen check.

The normal gate runs real-implementation Loom models at preemption bound 3.
Before accepting synchronization, ownership, pooling, waiter, or unsafe-memory
changes, run:

```sh
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=7 \
  cargo test -p observepass --lib --release
nix develop .#miri -c cargo miri test -p observepass --lib
```

A benchmark is evidence about performance, never evidence of correctness.
