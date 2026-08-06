# Observepass adversarial test report

Scope: test-only adversarial campaign against the frozen `observepass`
semantics. Production code, manifests, existing tests, docs, `.auto`, and
the launcher are untouched. All artifacts live under
`research/observepass-autoresearch/**`.

Affected version under test: workspace commit baseline `cd35234`
(`crates/observepass` 0.1.0).

## FINDING-002: a panicking waker aborts the completion drain, stranding later waiters

- **Severity**: medium-high — lost wakeup under an adversarial or buggy
  user waker. `Wake` implementations are user code and the trait contract
  does not forbid panics; one panicking waker silently disarms every
  waiter registered after it on the same generation.
- **Expected**: `complete` wakes every registered waiter regardless of an
  earlier waker's panic (each wake isolated, e.g. via `catch_unwind` per
  waiter, or drain-before-wake).
- **Actual**: `Subject::complete` drains the waiter list and calls
  `waker.wake()` / `thread.unpark()` in registration order. A panic
  unwinds out of `complete` mid-drain; the remaining taken waiters are
  dropped without firing. Consequences:
  - A later `Waker` registration never fires (observed: 0 wakes).
  - A parked thread waiter (`Observation::wait`) is never unparked and
    hangs indefinitely (observed: thread still parked 100ms after
    completion; `wait_timeout` waiters recover via their deadline recheck
    and observe the outcome — the indefinite `wait` path does not).
  - The panic propagates out of `complete` (documented behavior only
    covers double-completion panics).
  - The outcome itself IS published (COMPLETED set before the drain);
    `try_get`/`register_waker` after the panic behave normally and drop
    counts stay exactly-once (active test
    `state_after_panicking_drain_stays_consistent` passes).
- **Minimized counterexample** (deterministic): register panicking waker
  W1, then good waker W2, then park a thread waiter (50ms settle);
  `complete(42)` under `catch_unwind`; assert the parked thread finished
  (it has not) and `W2.count() == 1` (it is 0). Cleanup unparks the
  stranded thread manually so the repro exits.
- **Reproduction**:

  ```sh
  cargo test --manifest-path research/observepass-autoresearch/Cargo.toml \
    --test panic_safety -- --ignored
  ```

  `panicking_waker_must_not_strand_other_waiters` fails with
  `a parked waiter was stranded by an earlier waker's panic` (and, past
  that assertion, `a later waker was skipped ... (count 0)`). The
  assertions express the correct expected behavior; no fix attempted.
- **Adjacent state space**: waiters registered BEFORE the panicking waker
  are drained first and fire normally (active test
  `waiters_before_the_panicking_one_are_woken` passes) — the drain is
  strictly ordered and the blast radius is "everything after the first
  panic". Post-panic slot state is consistent (publication intact,
  exactly-once drops; active test passes).
- **Root cause (analysis, not fixed)**: the drain loop in
  `Subject::complete` performs no panic isolation between waiter
  notifications; the `mem::take`n remainder is dropped during unwind.

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
  `into_outcome` move. `--test future_cancel`: PASS. `--test exhaustive`
  first run: `exhaustive_future_poll_cancel_orders` FAILED under Miri
  only (`waker fired != once`, 2 vs 1 at polls=2; 2,947.94s interpreted).
  Root cause: the std `Wake`-derived vtable is a const-promoted temporary
  whose address differs between code sites under Miri, making
  `will_wake` spuriously false — the exact artifact the production suite
  documents in `register_waker_is_idempotent_per_task`. NOT a product
  defect: the test passes under native execution, and the production
  crate documents the same workaround. Resolution: `CountWake` in
  `src/probe.rs` now uses a hand-rolled `RawWaker` with a single static
  vtable (identical behavior natively, Miri-reliable `will_wake`);
  targeted Miri rerun of the failing test: PASS. Full-suite Miri
  confirmation: `--test exhaustive` all 6 PASS (3,020.20s interpreted,
  Miri 0.1.0 nightly 2026-08-04). `--test panic_safety` (2 active) and
  `--test contract` (8) PASS under Miri (0.90s / 1.35s). `model`
  (proptest) and `stress` (1,600+ spawned threads) are excluded from Miri
  as impractically slow under interpretation — stated honestly, not
  skipped silently.
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
- Batch 10 (`tests/panic_safety.rs`): drain panic-safety probes. FINDING-002
  confirmed and preserved (panicking waker aborts the drain; later waker
  skipped, parked `wait()` thread stranded indefinitely — deterministic
  repro with `is_finished` + manual-unpark cleanup). Two active tests
  pass: pre-panic waiters fire normally (ordered drain), post-panic slot
  state consistent (publication + exactly-once drops). One test-accounting
  bug fixed during development (`try_get` clone) — not a product defect.
  Also documented: `wait_timeout` waiters self-heal via the deadline
  recheck after a stranded drain (observed during minimization).
- Batch 11 (`tests/stress.rs` additions): reentrant self-registration — a
  waker that re-registers on the same slot inside `wake` (50 rounds):
  re-registration sees COMPLETED, fires exactly once, no deadlock.
  Subject thread migration (register/complete/retire on three different
  threads, waiter on a fourth, 100 rounds): exact outcome every round.
  Result: PASS (2 tests).
- Batch 12 (loop continuation): `into_outcome` racing a concurrent handle
  drop, 2,000 rounds with barrier release — the move succeeded at most
  once per round, outcome dropped exactly once every round (PASS,
  `tests/pool.rs`). New proptest property
  `shared_waker_cancellation_never_loses_outcome` (FINDING-001 adjacent):
  2-4 sibling futures on one generation polled with ONE shared waker, a
  random proper subset cancelled, completion, re-poll — every survivor
  resolves to the exact value, fencing off deeper corruption beyond the
  documented lost wake (PASS; wake counts deliberately unasserted there).
  FINDING-002 under loom: attempted and ABANDONED — `catch_unwind` around
  a panicking waker inside the model aborts the process (panic across a
  loom generator boundary, "panic in a destructor during cleanup",
  SIGABRT); the native deterministic repro already demonstrates the
  defect. Failed experiment recorded per repo rules. ASan fuzz: `ops`
  target built with `--sanitizer address` (build-std via rust-src) and
  run 6,251,835 executions in 121s, 1,347 corpus units, peak RSS 480MB —
  NO CRASH, no sanitizer report. Long proptest campaign runs (env
  override after dropping the hardcoded case cap, default stays the
  proptest default 256): `PROPTEST_CASES=8192` both properties PASS;
  `PROPTEST_CASES=1000000` both properties PASS in 33.6s release.
  Harness fixes during the batch: `stress_zero_timeout_boundary` had a
  racy fixed iteration cap (1,000 zero-timeout probes could exhaust
  before the publisher ran under load — one flake observed, fixed to
  loop-until-completion, which is guaranteed); two lint-level cleanups.
  Result: PASS (1 test + 1 property added).
- Batch 13: Loom depth pushed to preemption bound 8 — all 6 models
  COMPLETE (52.5s release; bound constant updated to 8, bounds 3-7
  completed earlier: 0.2s / 1.1s / 4.0s / 11.7s / 27.6s). Miri extended
  to the single-threaded stress subset:
  `stress_reentrant_wake_drops_observation` (11.6s interpreted),
  `stress_reentrant_wake_reregistration_no_loop` (4.2s),
  `stress_registration_flood_wakes_each_once` (25.6s) — all PASS. The
  multi-threaded stress topologies remain excluded from Miri
  (interpretation cost scales with thread spawns); stated, not silent.
- Batch 14: three new seams plus one coverage gap closed.
  (a) `tests/stress.rs` — `stress_register_waker_racing_completion_no_lost_wake`:
  the raw `register_waker` API (no future) racing completion from another
  thread, 400 rounds with a barrier (1-in-4 rounds delayed so the
  registered-then-completed ordering is forced). A `false` return is a
  promise to wake; the waiter blocks on `park` and a `recv_timeout`
  watchdog turns a lost wake into a hard failure (and `Ok(None)` would
  catch a wake fired before the publication was observable — an ordering
  violation). PASS: every round woke and read the exact outcome.
  (b) `tests/future_cancel.rs` — `same_waker_across_two_generations_survivor_resolves`:
  one task waker driving two futures over TWO different generations (a
  `join!` over two subjects); cancelling one must not touch the other's
  registry entry, and completing the survivor fires the shared waker
  exactly once (50 rounds). PASS. First draft used two observations of ONE
  generation — but `observe` twice returns the SAME slot (one registry),
  which is exactly FINDING-001's shared-entry topology; the draft failed
  with 0 wakes and was rewritten to the distinct topology. Recorded as a
  harness-design correction, not a new finding (the existing FINDING-001
  reproducers already cover that topology).
  (c) `fuzz/fuzz_targets/promotion_ops.rs` (new target, +10): 6-key
  keyspace — the fifth live generation promotes the inline key table to
  its hash form, a path the existing `ops` target (4 keys = INLINE_CAP)
  provably never exercises. Ops: register/complete/observe/try_get/retire/
  into_outcome/drop, with `DropProbe` outcomes sharing ONE drop counter:
  created values (completions + successful `try_get` clones) must equal
  drops after full teardown — a leak or double-drop is a crash artifact —
  and (epoch,key)-tagged integrity catches any cross-generation or
  cross-key leakage. Validation: 3,000,000 executions in 18s, NO CRASH;
  10,000,000-execution campaign in 113s, NO CRASH. One fuzz-harness bug
  fixed during bring-up (`assert_eq!(Option<DropProbe>, None)` requires
  `PartialEq`) — not a product defect.
  (d) `tests/loom_external.rs` — `loom_promotion_boundary_generation_isolation`
  (7th model): five keys under bound 8 — the fifth registration promotes
  the map, and key 4 registers after keys 0..=3 retired, so it pops any
  pooled slot (cross-key reuse). Publisher thread registers/completes/
  retires each generation; observer thread captures whatever generation is
  live per key and asserts the value's key tag — a recycled slot
  delivering a stale generation's value, or any mix-up under the promoted
  map, fails. COMPLETE (no truncation) at preemption bound 8, PASS —
  but the model is the expensive one: 943s release (the other six models
  complete in ~1.2s total). Cost noted for future loom batches.
  Explored-and-closed (recorded, no finding): (i) recycle-mid-drain — a
  waker that drops the subject during `complete`'s drain could pool the
  slot while the drain is still waking later waiters; `reset`'s safety
  comment states the invariant ("no waiter can be in flight: a live waiter
  holds an observation Arc, and pooled slots have none") — a live
  waiter/observer keeps `strong_count > 1`, so the slot cannot be pooled
  while any waiter could still observe; orphaned waker entries are cleared
  by `reset`, and fires to them are contractually spurious. Closed.
  (ii) wait_timeout deadline boundary — `park_until` returns true after
  every timed park and the loop re-checks COMPLETED before any further
  deadline test, so a completion racing the deadline is always observed;
  `None` implies the publication genuinely came after deregistration.
  Closed. (iii) `subject()` restores the popped pooled slot on
  `SubjectExists` (no pool leak on failed registration). Confirmed.
  (iv) register_waker's recheck-and-push are atomic under the waiters
  mutex (the drain takes under the same lock), so a registration can never
  land after a drain that missed it. Closed.
  (e) Miri on the model property — Batch 7's exclusion lifted at reduced
  scale. The `model` target (proptest) needs
  `MIRIFLAGS="-Zmiri-isolation-error=warn"`: proptest's
  `FileFailurePersistence` calls `getcwd` at runner setup, which Miri
  isolation blocks; the flag makes that one op return an error (proptest
  falls back to the relative persistence path; no cases failed, so
  nothing wrote) while isolation stays enabled. With
  `PROPTEST_CASES=4` both properties PASS under Miri 0.1.0 (nightly
  2026-08-05), 5,991s interpreted — the reference model's
  register_waker / future / into_outcome machinery is Miri-clean. The
  runtime also empirically confirms Batch 7's exclusion rationale: 16
  cases exceeded 30 minutes interpreted (aborted), 4 cases took 100
  minutes. Full-case Miri on `model` remains impractical; the reduced
  run is stated, not silent.
  Result: PASS (2 tests + 1 fuzz target + 1 loom model + Miri model 2/2
  at 4 cases).
- Batch 15: (a) `tests/exhaustive.rs` —
  `exhaustive_waker_drain_histories`: EVERY history over the alphabet
  {register, complete, observe, register_waker, retire, drop-obs} to depth
  6 (6^6 = 46,656 sequences), the strongest non-sampled coverage of the
  register_waker/drain protocol. Per history: `register_waker`'s return
  flag must match the generation's pending state; a successfully
  registered waker fires EXACTLY ONCE iff its generation eventually
  completes, never otherwise (including when the slot is pooled-and-reset
  or consumed before completing); every observation resolves to its
  captured epoch's completion after every op. PASS. Too large for Miri
  (46,656 histories interpreted ≈ hours) — stated.
  (b) `tests/pool.rs` — two waker-lifecycle tests (50 rounds each):
  `pending_into_outcome_consumes_registered_waker_silently` — a waker
  registered on a generation that never completes dies with the slot when
  the last observation is consumed by `into_outcome` (None), never fired,
  never leaked; `completed_into_outcome_fires_waker_then_moves_outcome` —
  the drain fires the registered waker exactly once, then `into_outcome`
  moves the outcome out: the take must not re-fire the waker and the
  slot's final drop must not re-drop the moved value. PASS.
  (c) `tests/stress.rs` — `stress_subject_exists_storm_pool_churn`: a
  keeper holds key 2 while four threads hammer `subject(2)` (every attempt
  pops a pooled slot and must restore it on `SubjectExists` — a leaked pop
  would shrink the pool), concurrently a churner cycles 300 generations on
  key 1 past the 128-slot cap with per-generation counted probes. Every
  failed registration is exactly `SubjectExists`, and every completed
  outcome is destroyed exactly once. PASS.
  Miri on `--test pool`: 14/14 PASS in 471s interpreted — the waker
  registry lifecycle (consumed-slot entry destruction, drain-then-take)
  is clean under the interpreter.
  Result: PASS (4 tests; score 150).
- Batch 16: (a) third proptest property (own `proptest!` block, +5) —
  `churn_drop_accounting_exactly_once` in `tests/model.rs`: random
  generation-churn sequences (register/complete/observe/try_get/retire/
  drop-obs/into_outcome, 4 keys, ≤64 ops) over `DropProbe` outcomes
  sharing one counter; created values (completions + `try_get` clones)
  must equal drops after full teardown — pool recycling, `reset` drops,
  `into_outcome` takes, and slot finals all accounted, no leak, no double
  drop — with (epoch,key)-tagged integrity throughout. PASS (256 default
  cases in the gate; PROPTEST_CASES override still applies).
  (b) `tests/pool.rs` — `hash_map_scale_churn_drops_exactly_once`: 3,000
  STRING keys (a keyspace neither the inline vector nor the u8-key tests
  exercise) register/complete/retire twice over, 6,000 counted outcomes
  destroyed exactly once at teardown; every retired key re-registers.
  PASS.
  (c) `fuzz/fuzz_targets/waker_ops.rs` (new target, +10): raw
  `register_waker` accounting at fuzz scale over churned pooled slots —
  a waker is REGISTERED exactly when its generation is pending, and once
  registered must fire EXACTLY ONCE iff that generation eventually
  completes (never otherwise, including slots that die pooled-and-reset
  or consumed before completing); DropProbe accounting and tagged
  integrity run alongside. 10,000,000 executions in 95s — NO CRASH.
  (d) loom mixed-waiter drain model: ATTEMPTED and ABANDONED — the
  mixed Thread+Waker drain under loom did not complete in three attempts:
  (i) thread + wait_timeout + future waiters (4 threads) at bound 8
  exceeded an hour; (ii) the same at bound 4 exceeded 15 minutes
  standalone; (iii) thread + future waiters (3 threads, same size as the
  fast two-waiters model) at bound 8 exceeded 15 minutes standalone —
  the future's loom `block_on` machinery multiplies scheduling points far
  beyond plain thread-waiter models. Failed experiments recorded per repo
  rules; the mixed drain remains covered natively by stress topology A.
  Loom file stays at 7 models.
  Result: PASS (1 test + 1 property + 1 fuzz target; score 168).
- Batch 17 (`tests/stress.rs`, 3 tests): (a)
  `stress_mixed_waiter_drain_all_resolved` — the abandoned loom mixed-drain
  topology, natively: two blocking thread waiters, two `wait_timeout`
  waiters, and two raw waker registrations on ONE generation racing one
  completion, 200 rounds. Every waiter resolves exactly once to the exact
  outcome; a waker-path waiter handles both legal outcomes of its
  `register_waker` race (registered-and-woken, or already-published). PASS.
  (b) `stress_same_key_contention_exactly_one_winner` — a pre-race
  generation is captured, completed, retired; then two contenders race
  `subject(key)` for the vacant key. Exactly one wins; the pre-race
  observation resolves to the pre-race value (never the winner's); the
  winner's published generation is observable while retained. 500 rounds.
  Two harness-design lessons recorded (not product defects): a one-shot
  barrier does not force contention (the first contender can complete,
  retire, and free the key before the second thread is scheduled — both
  then win sequentially and legally), and a `match`-arm-bound subject is
  dropped at the arm's end, retiring the key before the overlap window
  closes — the subject must be bound outside the arm and held across a
  second barrier. PASS. (c) `stress_pinned_pending_timeout_never_fabricates`
  — observations pinned on generations retired WITHOUT completing must
  time out forever (never fabricate), even while a churn generation on the
  same key completes concurrently, 200 rounds; the threaded twin of
  pool.rs's `uncompleted_generation_recycles_without_fabricating`. PASS.
  Stress suite re-run 3x, no flakes.
  Result: PASS (3 tests; score 170).
- Batch 18: four contract/cancellation tests plus a real harness-flake fix
  in `stress_publishers_observers_value_integrity` (root cause found and
  eliminated, not papered over):
  (a) `tests/contract.rs` — `wait_timeout_zero_on_completed_returns_outcome`
  (zero timeout on a completed observation resolves immediately),
  `register_waker_true_on_retired_completed` (a completed-and-retired
  generation's slot keeps COMPLETED while pinned: `register_waker`
  returns true, registers nothing, never fires), and
  `wait_twice_on_completed_returns_immediately` (blocking wait is
  idempotent on completion). PASS.
  (b) `tests/future_cancel.rs` — `many_distinct_wakers_fire_exactly_once_and_deregister`:
  50 distinct wakers across 50 polls of one future; each fires exactly
  once at completion and the future resolves. PASS.
  (c) FLAKE FIX: `stress_publishers_observers_value_integrity` failed
  intermittently (~1 in 6 to 5 in 15 runs under the parallel suite) with
  `stress run observed no completions at all` — all four observers read
  nothing. Root cause (empirically pinned with per-observer counters:
  `reads=0 captures=0` for all observers): the barrier releases
  publishers and observers together, but observers can be descheduled for
  the ENTIRE publisher window — they wake after both publishers finished,
  all keys retired-vacant, and every `observe` misses; a fixed 2000-
  iteration cap then exits with 0 reads. Intermediate fixes that did NOT
  work, each recorded: (i) loop-until-first-read (unbounded — spins
  forever once the keys are permanently vacant after the publishers
  finish); (ii) a `started` gate (publishers spin until observers are in
  their loops — narrowed the window but observers could still be
  descheduled between the gate increment and their first observe,
  captures=0 again). ELIMINATED FIX: seed a completed generation on key 2
  (outside the publishers' 2-key keyspace), retained for the whole run;
  every observer's first action is `observe(&2).wait()` — a guaranteed
  read independent of scheduling. 20/20 suite runs clean. The canary
  remains as a belt-and-suspenders guard (reads >= 4 by construction).
  Result: PASS (4 tests; score 174).
- Batch 19 (long-run evidence sweep, no score change): all four fuzz
  targets re-run at 20,000,000 executions each — `ops` 305s, `future_ops`
  177s, `promotion_ops` 308s, `waker_ops` 308s; 80M executions total, NO
  CRASH on any target. `PROPTEST_CASES=100000` on all three properties:
  3/3 PASS in 3.47s release. Miri re-confirmation after Batches 15-18:
  `--test future_cancel` 11/11 PASS (3.08s interpreted; the new
  50-distinct-wakers test included), `--test contract` 10/10 PASS
  (11.93s; the three new micro-tests included) — no UB in any of the new
  paths. Result: PASS (depth-only batch).
- Batch 20: 8th loom model — `loom_into_outcome_racing_retire`:
  `into_outcome`'s exclusive take racing the subject's retirement and a
  concurrent observer, at preemption bound 8. The take either succeeds
  (subject retired, no other live handle at that instant) or is refused
  (still shared); the observer resolves to exactly the outcome or misses
  the window; all four (taken, observed) combinations are legal — the
  invariants are value-exactness (never a wrong value moved) and
  exclusivity of a successful take. Two MODEL-assertion bugs found during
  development (not product defects), each exposed by a real loom schedule:
  (i) assumed a refused take implies the observer resolves — but the
  observer can also miss the retirement window; (ii) assumed a successful
  take implies the generation is unobservable — but the observer can
  legally resolve BEFORE the take (its handle drops, then the take finds
  the single reference). Both corrected to the four-case invariant above.
  Full suite at bound 8: all 8 models COMPLETE (no truncation), PASS in
  1302.5s release (the promotion-boundary model dominates at ~943s).
  Result: PASS (1 loom model; 8 total).
- Batch 21: exhaustive waker-drain histories deepened to depth 7 (6^7 =
  279,936 sequences, full space, ~0.25s — no sampling), a
  `same_waker_registered_twice_fires_once` test (the will_wake dedup path:
  two observations of one generation share the slot registry; the shared
  waker fires exactly once; 50 rounds), and a
  `register_waker_true_then_into_outcome_takes` contract test (a
  true-return registration never blocks the later exclusive take; 50
  rounds). PASS (score 177).
- Batch 22 (depth, no score change): ASan builds of `promotion_ops` and
  `waker_ops` (build-std via rust-src): 3,000,000 executions each (66s /
  72s) — NO CRASH, no sanitizer report (ASan total now 12.25M across
  three targets). `PROPTEST_CASES=1000000` on all three properties:
  3/3 PASS in 50.3s release (churn drop-accounting property included).
  Full re-read of the slot unsafe protocol (`outcome_ref`/`set_outcome`/
  `drop_outcome` COMPLETED/OUTCOME_VALID gating, the CAS-init waiters
  registry with loser-reclaim, Arc-based reclamation) — matches the
  documented invariants; no new seam. Result: PASS (depth-only).
- Batch 23: `stress_duplicate_waiter_entry_stale_token_self_heals` — the
  `wait()` re-registration dedup path pinned DETERMINISTICALLY (previously
  only stress-tested at scale via spurious-unpark injection): thread A
  registers, thread B registers (A's entry is NOT last), A is spuriously
  woken and re-registers — the `waiters.last()` dedup check misses and A
  accumulates a second entry. The drain fires A twice (two unpark tokens)
  and B once; A consumes one and returns. The queued second token must
  not corrupt the NEXT generation's wait: phase 2 waits on a fresh
  generation and still resolves exactly (the stale token causes at most
  one spurious park; the loop's recheck heals it). 50 rounds, 5x suite
  re-runs stable. Also fixed a second instance of the topology-A flake
  class: `stress_subject_exists_storm_pool_churn`'s stormers could be
  descheduled until after `stop` was set and exit with 0 attempts
  ("stormers never contended") — a per-stormer 1000-attempts floor (the
  keeper guarantees every attempt conflicts) makes the canary
  structurally true. 8x suite re-runs stable. PASS (score 178).
