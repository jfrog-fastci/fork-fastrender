//! Unit tests for the paint module.
//!
//! Historically these lived under `tests/paint/` and were pulled into a single integration-test
//! harness (`tests/paint_tests.rs`). Keeping them as unit tests makes them faster to compile and
//! allows `cargo test --lib` to exercise paint regressions.

mod backdrop;
mod iframe_embedder;
mod legacy;
mod paint;
