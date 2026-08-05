# observepass developer recipes.

# Full frozen correctness gate (fmt, workspace tests, clippy -D warnings,
# frozen loom model, frozen-diff).
gate:
    bash .auto/checks.sh

# Frozen workload measurement (best-of-5, primary metric).
measure:
    bash autoresearch.sh

# Real-implementation loom models (11 models, preemptions 3).
loom:
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p observepass --lib --release

# Real-implementation loom models, deeper exploration (preemptions 7).
loom-deep:
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=7 cargo test -p observepass --lib --release

# Miri over the real-thread tests (needs the miri devShell: nix develop .#miri).
miri:
    nix develop .#miri -c cargo miri test -p observepass --lib

# The verification suite in order.
verify:
    just gate
    just loom
    just loom-deep
    just miri
