#!/usr/bin/env bash
set -euo pipefail
base=$(cat .auto/BASELINE)
git diff --quiet "$base" -- crates Cargo.toml Cargo.lock README.md docs AGENTS.md || { echo "CHECK FAIL: production changed"; exit 1; }
git diff --quiet "$base" -- .auto ':!.auto/BASELINE' autoresearch.sh || { echo "CHECK FAIL: research rules changed"; exit 1; }
if test -f research/observepass-autoresearch/Cargo.toml; then cargo test --manifest-path research/observepass-autoresearch/Cargo.toml --all-targets --no-fail-fast; cargo clippy --manifest-path research/observepass-autoresearch/Cargo.toml --all-targets -- -D warnings; fi
echo "CHECK OK"
