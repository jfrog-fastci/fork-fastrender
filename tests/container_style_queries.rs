// Compatibility wrapper for running the container style query regression tests directly.
//
// Most style regressions live under `tests/style/` and are pulled into the `style_tests` harness
// (see `tests/style_tests.rs`). Some workflows/docs reference this file as an individual integration
// test target (e.g. `cargo test --test container_style_queries`), so keep this thin wrapper to
// preserve that entrypoint without duplicating test code.

#[path = "style/container_style_queries.rs"]
mod container_style_queries;
