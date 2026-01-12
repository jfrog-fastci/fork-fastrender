//! Allocation-failure regression tests.
//!
//! These tests install a custom global allocator so we can force specific allocations to
//! fail and validate that the renderer handles OOMs gracefully. Because a Rust crate can only
//! define a single global allocator, these tests must live in their own harness.
//!
//! Note: this file uses `include!` (instead of `mod allocation_failure;`) to load the module in
//! `tests/allocation_failure/mod.rs`. That avoids a Rust module-name collision between the harness
//! file (`tests/allocation_failure.rs`) and the module directory (`tests/allocation_failure/`),
//! without reintroducing forbidden `#[path = "..."]` shims.

mod allocation_failure_tests;
