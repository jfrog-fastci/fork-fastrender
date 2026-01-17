//! Allocation-failure regression tests.
//!
//! These tests install a custom global allocator so we can force specific allocations to
//! fail and validate that the renderer handles OOMs gracefully. Because a Rust crate can only
//! define a single global allocator, these tests must live in their own harness.
//!
//! Note: the test modules live under `tests/allocation_failure_tests/` and are pulled in via a
//! normal Rust `mod` declaration. This keeps the `allocation_failure` test-binary name while
//! avoiding forbidden `#[path = "..."]` shims.
#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

mod allocation_failure_tests;
