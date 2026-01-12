// Aggregator for image integration tests under tests/image_integration/.
//
// Cargo only executes integration test crates at the root of `tests/`, so this harness pulls the
// nested modules into a single test crate. The unified `tests/integration.rs` harness also
// includes these tests, but some scripts still invoke this target directly.

mod common;
mod image_integration;
