//! Browser UI / worker integration tests.
//!
//! Historically these lived in `tests/browser_integration_tests.rs` as their own integration test
//! crate. The wider repository has since grown a unified integration test binary in
//! `tests/integration.rs`, but some automation (and agent tasks) still expects a
//! `browser_integration_tests` test target.
//!
//! Keep this shim so `cargo test --test browser_integration_tests` continues to work.

mod browser_integration;

