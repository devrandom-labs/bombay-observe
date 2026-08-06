# Observepass adversarial test-only autoresearch

Write only under `research/observepass-autoresearch/**`. Never touch or fix
production, manifests, existing tests, docs, `.auto`, or the launcher.

Attack observe/complete races, exactly-once initialized publication, same-key
subject contention, stale retirement, pooled-slot generation leakage, waiter
wakeup and cancellation, timeouts at boundaries, move-only outcomes, drop
counts, reentrant wake/drop behavior, map promotion, retention and reclamation.
Use independent models, exhaustive histories, proptest, fuzzing, deterministic
stress, bounded real-protocol Loom, and Miri.

Minimize each defect into `#[ignore = "FINDING-NNN: reason"]`, document
`## FINDING-NNN` in `RESEARCH-REPORT.md` with exact reproduction and never fix
it. Keep passing tests active and report exact bounds, seeds, executions, and
interrupted runs.
