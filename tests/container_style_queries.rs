// Compatibility wrapper for running the container style query regression tests directly.
//
// The canonical home for these tests is `tests/style/container_style_queries.rs`, which is
// compiled into the main integration test harness (`tests/integration.rs`). Some workflows/docs
// reference this file as an individual integration test target (e.g.
// `cargo test --test container_style_queries`), so keep this thin wrapper to preserve that
// entrypoint without duplicating test code.

#[path = "style/container_style_queries.rs"]
mod container_style_queries;
