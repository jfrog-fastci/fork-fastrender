// Aggregator for paint-related integration tests under tests/paint/ (and related directories).
// Cargo only executes integration test crates at the root of `tests/`, so this
// harness pulls the nested modules into a single test crate.

mod common;

mod backdrop;
mod paint;
mod r#ref;
