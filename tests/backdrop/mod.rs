//! Backdrop filter-related test modules.
//!
//! Most backdrop regressions live under `tests/paint/` and are aggregated by `tests/paint_tests.rs`.
//! This directory historically hosted backdrop-only tests; many have since been promoted to
//! standalone `backdrop_root_*_test.rs` targets for faster iteration.
//!
//! Keep the module in place so the `tests/backdrop_tests.rs` harness remains a valid integration
//! test crate even when it does not currently include additional modules.
