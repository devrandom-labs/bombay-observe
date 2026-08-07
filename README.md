# Bombay Observe

[![CI](https://github.com/devrandom-labs/bombay-observe/actions/workflows/checks.yml/badge.svg)](https://github.com/devrandom-labs/bombay-observe/actions/workflows/checks.yml)
[![Crates.io](https://img.shields.io/crates/v/bombay-observe.svg)](https://crates.io/crates/bombay-observe)
[![Documentation](https://docs.rs/bombay-observe/badge.svg)](https://docs.rs/bombay-observe)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Generation-safe completion publication and observation for concurrent Rust.

The package is published as `bombay-observe` and imported as `observe`:

```toml
[dependencies]
observe = { package = "bombay-observe", version = "0.1" }
```

## Why

Concurrent systems often need to publish one terminal outcome and let any
number of observers retrieve it, whether they arrive before, during, or after
completion. Bombay Observe provides that primitive without embedding runtime,
task-tree, transport, or application-specific concepts.

Its guarantees are:

- completion racing observation cannot lose the outcome;
- generations sharing a key remain isolated;
- retiring an old generation cannot affect its replacement;
- cancelling synchronous or asynchronous observers is safe;
- retained state is bounded by explicit ownership;
- a panicking user waker cannot strand other waiters.

## Example

```rust
use observe::ObservationSpace;

let space = ObservationSpace::<u64, &'static str>::new();
let mut subject = space.subject(7)?;
let observation = space.observe(&7)?;

subject.complete("done");
assert_eq!(observation.wait(), "done");
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Observation` also implements `IntoFuture`, so it can be awaited directly.
The crate does not depend on an async runtime.

## Development

The repository uses the same pinned Rust/Nix structure as Nexus. Enter the
development environment with `nix develop` and run the complete gate with:

```sh
nix flake check
```

The concurrency implementation is additionally exercised with Loom:

```sh
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 \
  cargo test -p bombay-observe --lib --release
```

## Documentation

API documentation is available on [docs.rs](https://docs.rs/bombay-observe)
and is also deployed to
[GitHub Pages](https://devrandom-labs.github.io/bombay-observe/).

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE), or
- [MIT License](LICENSE-MIT)

at your option.
