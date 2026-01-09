//! Aggregator for regression tests under tests/regression/.
//!
//! Cargo only executes integration test crates at the root of `tests/`, so this
//! harness pulls the nested modules into a single test crate.

mod r#ref;
mod regression;
