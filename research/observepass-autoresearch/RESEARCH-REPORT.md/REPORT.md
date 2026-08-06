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
- Batch 3 (`tests/model.rs`): independent sequential reference model
  (HashMap-of-epochs; per-key epoch counters; retired-epoch outcomes)
  driven by proptest: 256 cases per run, ops weighted across register /
  complete / observe / try_get / register_waker / into_outcome / retire /
  drop-obs / new/poll/migrate/cancel-future, keyspace 8 (2x INLINE_CAP,
  exercises inline and promoted maps), handle ids 0..16. Exact wake counts
  asserted at every completion (each distinct registered waker fires
  exactly once). Proptest config: `ProptestConfig::with_cases(256)`,
  default entropy (no fixed seed; failure persistence inactive for test
  targets). Shared-waker-across-futures topology deliberately excluded
  (deterministically hits FINDING-001, preserved separately). Result:
  PASS. Two model bugs found and fixed during development (subject-side
  refcount is 2 — map entry + Subject handle; exclusivity test must run
  after handle removal against 0 remaining refs) — not product defects.
- Batch 4 (`tests/exhaustive.rs`): exhaustive small-state exploration, no
  sampling. All single-key histories to depth 5 over the 5-op alphabet
  {register, complete, observe, retire, drop-obs} (3,125 sequences) and to
  depth 7 over the reduced 4-op alphabet (16,384 sequences), checked
  against a minimal model after EVERY op. All 6 drop-order permutations of
  {subject, observation, future} x {completed, pending} for exact drop and
  wake counts. All retirement subsets of 3 keys across the inline->hash
  promotion boundary (INLINE_CAP=4, 6 keys) with re-registration; all
  ordered double-retirement pairs; all poll-count/completion/cancellation
  orderings for futures. Result: PASS (6 tests, ~19.5k histories + all
  permutations).
- Batch 5 (`tests/stress.rs`): deterministic adversarial stress, real
  threads, barrier-synchronized, fixed-seed SplitMix64 op selection.
  Topology A: 2 publishers x 2 keys x 2,000 rounds vs 4 observers x 2,000
  iterations (try_get / wait_timeout(50ms) / register-and-drop cancellation
  storm / blocking wait), outcome tags checked per read (publisher, round,
  key encoding) — 0 wrong-tag reads. Topology B: 8 waiters fanned out on
  one generation x 200 rounds, exact-outcome assertion — 0 failures.
  Topology C: wait_timeout(0)/wait_timeout(1ns) boundary storm vs
  completion x 500 rounds — every Some carried the exact outcome, every
  round observed completion. Reentrant wake test: a waker whose wake()
  drops another observation of the same generation during the drain —
  100 rounds, fired exactly once each, no deadlock. Registration flood:
  256 distinct wakers, each fired exactly once. Result: PASS (5 tests).
- Batch 6 (`tests/loom_external.rs`): bounded Loom models over the real
  protocol from the public API (the dependency rebuilds with
  `RUSTFLAGS="--cfg loom"`; the research crate declares its own
  `check-cfg` and uses loom 0.7 with the `futures` feature). Six models:
  observe across retire/re-register (captured generation's outcome never
  lost), pooled-slot reuse with waiter traffic on the same slot memory,
  `ObservationFuture` under `loom::future::block_on` racing completion,
  concurrent subject contention (exactly one winner, observable
  completion), two waiters racing one completion (both woken), and the
  wait_timeout completed path. Preemption bound 7: all 6 models COMPLETED
  (no truncation) in 27.6s; bounds 3-6 also completed (0.2s / 1.1s / 4.0s
  / 11.7s). Run: `RUSTFLAGS="--cfg loom" cargo test --manifest-path
  research/observepass-autoresearch/Cargo.toml --release --test
  loom_external`. One test bug fixed during development (vacancy window
  between retire and re-register not modelled) — not a product defect.
  Result: PASS.
- Batch 7 (Miri, ownership/memory validity): `nix develop .#miri --command
  cargo miri test --manifest-path
  research/observepass-autoresearch/Cargo.toml --test pool` — all 11
  pool/drop-count tests PASS under Miri 0.1.0 (nightly 2026-08-04),
  27.3s interpreted. No UB in the UnsafeCell outcome protocol, raw
  waiters-pointer reclamation, Arc-based slot recycling, or
  `into_outcome` move. `--test future_cancel`: PASS. `--test exhaustive`:
  one test reported FAILED after 2,947.94s interpreted (5 passed, 1
  failed); the failing test and error are being isolated — see follow-up
  log entries.
- Batch 8 (coverage-guided fuzzing): cargo-fuzz 0.13.2 (nixpkgs) +
  libfuzzer-sys 0.4.13, `--sanitizer none`, nightly 2026-07-29 toolchain.
  Two targets under `fuzz/fuzz_targets/`: `ops` (sequential op
  interpreter, (epoch,key)-tagged value integrity, 4 keys, ≤256 ops per
  input) and `future_ops` (future/poll/migrate/cancel sequences with
  exact wake-count and cancelled-waker-silence assertions, 2 keys, ≤128
  ops). Runs: `ops` 10,663,376 executions in 91s (3,946 corpus units,
  peak RSS 28MB) — NO CRASH. `future_ops` 12,207,925 executions in 91s
  (2,338 corpus units, peak RSS 28MB) — NO CRASH. One fuzz-target bug
  found and fixed during bring-up (`chunks(2)` trailing-byte panic in the
  harness, input `[10]`) — not a product defect. The shared-waker
  topology is deliberately excluded from `future_ops` (FINDING-001,
  preserved separately). Corpus not committed (regenerable; 4.3MB).
- Batch 9 (`tests/contract.rs`, `tests/stress.rs` additions): API-contract
  defenses — double-complete panics AND the first outcome survives the
  attempted second publication; `SubjectExists`/`UnknownSubject` carry the
  key; `wait_timeout(Duration::MAX)` panics (documented Instant overflow);
  huge valid timeout on a completed observation returns immediately;
  move-only (non-Clone) outcome flows through `into_outcome` exactly
  once; shared slot refuses `into_outcome`; cloned spaces share the
  namespace across drops. Spurious-unpark injection stress: external
  thread fires 64 injected unparks at a blocked waiter while a second
  waiter registers, 100 rounds — every waiter resolved to the exact
  outcome, none left parked. Result: PASS (8 contract + 1 stress tests).
