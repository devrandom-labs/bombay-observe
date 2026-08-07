# Bombay Observe developer recipes.

check:
    nix flake check

test:
    cargo test --workspace --all-targets

loom:
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p bombay-observe --lib --release

deny:
    cargo deny check

docs:
    cargo doc -p bombay-observe --no-deps
