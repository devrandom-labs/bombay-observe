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

