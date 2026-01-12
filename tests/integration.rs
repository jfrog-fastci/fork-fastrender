//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate pulls the
//! integration test module trees under `tests/` into a single binary so the suite links once.

mod common;
mod api;
mod accessibility;
mod interaction;
mod fixtures;
mod browser_integration;
mod guards;
mod interaction;
mod js;
mod interaction;
mod resource;
mod wpt;

// Keep the reference image comparison helpers available for fixture-style tests.
#[allow(dead_code)]
mod r#ref;
mod tooling;

// Regression tests under `tests/regression/`.
mod regression;
mod bin;
mod bundled;

mod wpt;
