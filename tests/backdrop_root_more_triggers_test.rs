// Separate integration test crate so we can run this regression in isolation without compiling
// the full `paint_tests` harness.
#[path = "paint/backdrop_root_more_triggers_test.rs"]
mod backdrop_root_more_triggers_test;

