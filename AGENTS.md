# Observepass research rules

Observepass is an actor-independent concurrent completion-observation
mechanism. It knows keys, generations, outcomes, subjects, and observers. It
must not know actors, parents, children, supervisors, addresses, mailboxes,
Tokio, KERI, Zenoh, or Nexus.

The frozen semantics are inviolable: completion must not be lost when observe
races complete; each generation is isolated; stale retirement cannot affect a
replacement; cancellation is safe; retention is bounded by explicit ownership.
Never weaken tests or measurements. Research primary papers and production
implementations, record failed experiments, and prefer safe Rust. Any unsafe
experiment requires stated invariants plus Loom and Miri evidence.

