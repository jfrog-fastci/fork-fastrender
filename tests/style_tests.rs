// Minimal harness for running targeted style regression tests as a standalone integration test.
//
// The full integration suite is compiled into `tests/integration.rs`, but some workflows (and
// downstream tooling) still expect to run individual style tests via `--test style_tests`.
//
// Keep this file lightweight to avoid duplicating the entire style suite compilation.

#[path = "../src/style/tests/style/defined_customized_builtin_integration_test.rs"]
mod defined_customized_builtin_integration_test;

#[path = "../src/style/tests/style/hostile_css_no_panic_smoke_test.rs"]
mod hostile_css_no_panic_smoke_test;
