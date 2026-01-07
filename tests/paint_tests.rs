// Aggregator for paint regression tests under tests/paint/.
// Cargo only executes integration test crates at the root of `tests/`, so this
// harness pulls the nested modules into a single test crate.

#[path = "paint/rayon_test_util.rs"]
mod rayon_test_util;

mod paint;
mod r#ref;
