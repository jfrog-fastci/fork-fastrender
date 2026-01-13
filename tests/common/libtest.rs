//! Helpers for interacting with Rust's built-in libtest harness.
//!
//! Integration tests in this repository frequently need to spawn a *child* copy of the current test
//! binary (e.g. to apply an irreversible OS sandbox). When doing so, we typically pass `--exact
//! <test_name>` so the child runs only one test function.
//!
//! `module_path!()` includes the crate name (e.g. `integration::...`), but libtest's reported test
//! names do **not**. These helpers normalize paths so `--exact` targets the intended test.

/// Strip the leading crate name segment from a `module_path!()` string.
pub(crate) fn strip_crate_name(module_path: &'static str) -> &'static str {
  module_path
    .split_once("::")
    .map(|(_, rest)| rest)
    .unwrap_or(module_path)
}

/// Construct the full libtest test name (`module::path::test_fn`) for use with `--exact`.
pub(crate) fn exact_test_name(module_path: &'static str, test_fn: &'static str) -> String {
  format!("{}::{}", strip_crate_name(module_path), test_fn)
}

