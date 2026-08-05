#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# The real loom models live in src/loom_tests.rs and are compiled into the
# lib when built with --cfg loom. --test loom is only a loom-harness smoke
# test; the lib run is what exercises the implementation's models.
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p observepass --test loom --lib --release
git diff --quiet baseline -- crates/observepass/tests crates/observepass/benches crates/observepass-perf .auto/checks.sh .auto/measure.sh || {
  echo "CHECK FAIL: frozen semantics or measurement changed"; exit 1;
}
echo "CHECK OK"

