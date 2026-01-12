// Compatibility wrapper for running the scroll-state container query regression tests directly.
//
// The canonical home for these tests is `tests/style/container_scroll_state_queries_test.rs`,
// which is compiled into the main integration test harness (`tests/integration.rs`). This file
// exists so commands like `cargo test --test container_scroll_state_queries_test` work without
// needing to know about the module layout.

#[path = "style/container_scroll_state_queries_test.rs"]
mod container_scroll_state_queries_test;
