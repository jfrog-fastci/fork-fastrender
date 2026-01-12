//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate is the
//! long-term home for all "normal" integration tests; it pulls in module trees under `tests/` so
//! the suite links once.

mod common;
mod api;
mod accessibility;
mod fixtures;
mod guards;

// Keep the reference image comparison helpers available for fixture-style tests.
#[allow(dead_code)]
mod r#ref;
mod tooling;

// Regression tests under `tests/regression/`.
mod regression;

mod svg_integration;

#[test]
fn llvm18_statepoint_fixture_emits_verified_stackmaps() {
  tooling::llvm_stackmaps::llvm18_statepoint_fixture_emits_verified_stackmaps();
}
