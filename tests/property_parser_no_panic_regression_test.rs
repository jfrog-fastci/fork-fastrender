// Targeted regression tests for CSS property parsing robustness.
//
// This is kept as a standalone integration test target (instead of being compiled as part of the
// `style_tests` mega-suite) so it can be run in isolation without pulling in thousands of unrelated
// style regression modules.
//
// The actual test cases live under `tests/style/` so they remain colocated with the other style
// tests; this crate just wires them up as a dedicated `cargo test --test ...` target.

#[path = "style/property_parser_no_panic_regression_test.rs"]
mod property_parser_no_panic_regression_test;

