#!/usr/bin/env bash
set -euo pipefail
export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
cargo build -q -p observepass-perf --release

# Full-matrix measurement: the perf binary emits one METRIC line per metric
# per invocation. We take SAMPLES repeated samples, report each metric's
# median (the decision value) with its min..max range, and preserve every raw
# sample under target/measure-raw/ so variance can be audited after the fact.
SAMPLES=5
RAW_DIR="target/measure-raw"
mkdir -p "$RAW_DIR"
RAW_FILE="$RAW_DIR/$(date +%Y%m%d-%H%M%S).log"
: > "$RAW_FILE"

for _ in $(seq 1 "$SAMPLES"); do
  ./target/release/observepass-perf >> "$RAW_FILE"
done

awk -v raw="$RAW_FILE" '
/METRIC / {
  key = $2; sub(/=.*/, "", key);
  value = $2; sub(/^[^=]*=/, "", value);
  vals[key] = vals[key] " " value;
}
END {
  for (key in vals) {
    n = split(vals[key], a, " ");
    for (i = 2; i <= n; i++) {
      v = a[i]; j = i - 1;
      while (j >= 1 && a[j] > v) { a[j + 1] = a[j]; j--; }
      a[j + 1] = v;
    }
    median = a[int((n + 1) / 2)];
    printf "METRIC %s=%s\n", key, median;
    printf "ASI %s_range_min_max=%s..%s\n", key, a[1], a[n];
  }
  printf "ASI raw_samples=%s\n", raw;
}
' "$RAW_FILE" | sort
