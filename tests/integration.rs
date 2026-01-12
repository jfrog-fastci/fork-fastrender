//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate is the
//! long-term home for all "normal" integration tests; it pulls in module trees under `tests/` so
//! the suite links once.

mod common;
mod api;
mod fixtures;

// Keep the reference image comparison helpers available for fixture-style tests.
#[allow(dead_code)]
mod r#ref;
