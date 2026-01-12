// Compatibility wrapper for running the scroll-state container query regression tests directly.
//
// The canonical home for these tests is `tests/style/container_scroll_state_queries_test.rs`, which
// is included in the `style_tests` harness. This file exists so commands like:
//   cargo test --test container_scroll_state_queries_test
// work without needing to know about the aggregator layout.

#[path = "style/container_scroll_state_queries_test.rs"]
mod container_scroll_state_queries_test;
