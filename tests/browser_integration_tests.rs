//! Browser UI / worker integration test target.
//!
//! The repository now uses `tests/integration.rs` as a unified integration-test binary, but some
//! automation (and agent tasks) still expects a `browser_integration_tests` test target.
//!
//! Keep this shim so `cargo test --features browser_ui --test browser_integration_tests ...`
//! continues to work.

#![cfg(feature = "browser_ui")]

mod browser_integration;

