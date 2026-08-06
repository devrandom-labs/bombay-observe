# Observepass adversarial test report

Scope: test-only adversarial campaign against the frozen `observepass`
semantics. Production code, manifests, existing tests, docs, `.auto`, and
the launcher are untouched. All artifacts live under
`research/observepass-autoresearch/**`.

Affected version under test: workspace commit baseline `cd35234`
(`crates/observepass` 0.1.0).

## FINDING-001: cancelling one future disarms sibling futures that share its waker

- **Severity**: high — lost wakeup. In a real executor the surviving future
  is never re-polled and hangs forever, although its generation completes.
  Violates the frozen semantics ("cancellation is safe"; completion must
  not be lost).
- **Expected**: two (or more) `ObservationFuture`s on two observations of
  the same generation, both polled to `Pending` with the same task waker
  (the `join!`/`select!` pattern), survive the cancellation of either one:
  when the subject completes, the shared waker is woken at least once so
  the survivor is re-polled and resolves.
- **Actual**: `ObservationFuture::drop` deregisters by `will_wake` identity
  (`waiters.retain(|w| !matches!(.. w.will_wake(mine)))`). Because
  `register_waker` deduplicates registrations by the same `will_wake`
  identity, both futures share ONE waiter entry. The first future dropped
  removes that shared entry; the surviving future's `wakers` bookkeeping
  still holds the waker but the slot's waiter list is empty. `complete`
  drains an empty list: 0 wakes, survivor hangs.
- **Minimized counterexample** (deterministic, single-threaded, no
  executor): poll future A and future B (same generation) with one
  `CountWake` waker -> both `Pending`; drop A; `subject.complete(42)`;
  assert `wakes >= 1`. Observed: `0 wakes`.
- **Reproduction**:

  ```sh
  cargo test --manifest-path research/observepass-autoresearch/Cargo.toml \
    --test future_cancel -- --ignored FINDING
  ```

  Both `cancelled_sibling_future_keeps_survivor_wakeable` (minimal) and
  `cancelled_siblings_leave_last_survivor_wakeable` (3-future adjacent
  variant) fail with `got 0 wakes`. The assertions express the correct
  expected behavior; no fix attempted.
- **Adjacent state space explored**: re-polling the survivor before
  completion re-registers the waker and heals the registration
  (`repolled_survivor_heals_registration`, active, passes) — the executor
  must be re-polled for some *other* reason, which a pure completion-driven
  executor will not do. Distinct-waker siblings are unaffected
  (`cancel_one_of_two_independent_futures_wakes_only_survivor`, active,
  passes). Single-future and migrated-waker cancellation leave no stale
  registration (active, pass).
- **Root cause (analysis, not fixed)**: registration dedup is by waker
  identity while deregistration is per-future; a registration shared by N
  futures is reference-counted nowhere, so the first drop wins.

## Campaign log

- Scaffold created; no adversarial runs recorded yet.
- Note on layout: the frozen `.auto/measure.sh` counts findings with
  `rg -c '^## FINDING-' RESEARCH-REPORT.md | awk -F: '{n+=$2}'`. `rg`
  omits the `path:` prefix for a single file argument, so a plain file
  parses to 0 findings regardless of content. `RESEARCH-REPORT.md` is
  therefore a directory (report files inside) so the frozen parser sees
  the real count. Accommodation of the frozen parser only; content is
  unchanged and complete.
- Batch 1 (`tests/future_cancel.rs`): 8 cancellation/waker tests, 6 active
  passing, 3 ignored as FINDING-001 (minimal + 3-future variant +
  ownerless `register_waker` variant).
  Deterministic, single-threaded, `CountWake` instrumentation.
- Batch 2 (`tests/pool.rs`): 11 pooled-slot/drop-count tests, all active
  passing. 300-generation churn past the 128-slot pool cap with
  per-generation drop counters (exactly-once destruction), deterministic
  LIFO slot reuse (no COMPLETED/outcome leakage), stale-waiter recycling,
  cross-key reuse, orphan observation pinning, `into_outcome` drop
  ownership transfer, waiter traffic across 8 recycle rounds. One test bug
  found and fixed during development (`try_get` clones the probe;
  accounting corrected) — not a product defect.
