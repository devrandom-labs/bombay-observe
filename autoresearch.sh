#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# The complete workload matrix in one invocation: primary observations/s,
# observe-first / complete-first shapes, retire/recreate, pooled-slot reuse,
# cancellation, fanout at 1/2/4/8, hot-path and wait-round-trip p50/p99,
# contention 1/2/4/8/16, per-op allocations, and retained bytes. Repeated
# samples with medians and ranges; raw samples preserved under
# target/measure-raw/. See .auto/measure.sh for the methodology.
bash .auto/measure.sh
