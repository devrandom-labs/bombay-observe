#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# The real loom models live in src/loom_tests.rs and are compiled into the
# lib when built with --cfg loom. --test loom is only a loom-harness smoke
# test; the lib run is what exercises the implementation's models.
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p observepass --test loom --lib --release
# Autoresearch may change the implementation, but never its oracle, model,
# measurement harness, or gate. In particular, the meaningful unit and Loom
# suites live under src/ rather than only under tests/.
git diff --quiet baseline -- \
  crates/observepass/src/tests.rs \
  crates/observepass/src/loom_tests.rs \
  crates/observepass/tests \
  crates/observepass/benches \
  crates/observepass-perf \
  .auto/checks.sh \
  .auto/measure.sh \
  .auto/prompt.md || {
  echo "CHECK FAIL: frozen semantics or measurement changed"; exit 1;
}
echo "CHECK OK"
