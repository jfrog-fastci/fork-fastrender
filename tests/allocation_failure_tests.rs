//! Allocation-failure regression tests.
//!
//! These tests install a custom `#[global_allocator]` so we can force specific allocations to
//! fail and validate that the renderer handles OOMs gracefully. Because a Rust crate can only
//! define a single global allocator, these tests must live in their own harness.

mod allocation_failure;
