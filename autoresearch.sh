#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"

# Frozen workload binary plus the extended measurement harness (release).
cargo build -q --release -p observepass-perf -p observepass-harness

# Primary metric: the frozen sequential workload, best of 5 runs — the exact
# methodology of .auto/measure.sh, relabelled observations_per_second.
best=0
for _ in 1 2 3 4 5; do
  out=$(./target/release/observepass-perf)
  score=$(printf '%s\n' "$out" | sed -n 's/^SCORE=//p')
  if awk -v a="$score" -v b="$best" 'BEGIN { exit !(a>b) }'; then best=$score; fi
done
if [ "$best" = 0 ]; then
  echo "harness failed: no SCORE parsed from observepass-perf" >&2
  exit 1
fi
printf 'METRIC observations_per_second=%s\n' "$best"

# Secondary metrics: throughput variants, p50/p99 latency, contention scaling,
# fanout, and observer cancellation.
./target/release/observepass-harness

# Allocation and retained-memory accounting (dhat global allocator).
./target/release/observepass-alloc
