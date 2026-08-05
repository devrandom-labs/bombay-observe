#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p observepass --test loom --release
git diff --quiet baseline -- crates/observepass/tests crates/observepass/benches crates/observepass-perf .auto/checks.sh .auto/measure.sh || {
  echo "CHECK FAIL: frozen semantics or measurement changed"; exit 1;
}
echo "CHECK OK"

