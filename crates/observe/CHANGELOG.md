# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Add `affine_pair` and `AffineObservation` for awaiting and moving a unique,
  non-`Clone` outcome from the typed observation slot.

### Changed

- Store the first waiter inline and replace a future's prior waker on task
  migration, eliminating stale registrations and an allocation on the
  single-waiter path.

## [0.1.1](https://github.com/devrandom-labs/bombay-observe/compare/bombay-observe-v0.1.0...bombay-observe-v0.1.1) - 2026-08-17

### Other

- add unkeyed publication pair ([#2](https://github.com/devrandom-labs/bombay-observe/pull/2))
