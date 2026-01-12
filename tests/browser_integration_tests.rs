//! Browser UI / worker integration test target.
//!
//! The repository also provides a unified integration test binary (`tests/integration.rs`), but
//! some automation (and agent tasks) still expects to run:
//! `cargo test --features browser_ui --test browser_integration_tests ...`
//!
//! Keep this shim so that command continues to work.

#![cfg(feature = "browser_ui")]

mod browser_integration;

