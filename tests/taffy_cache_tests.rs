//! Aggregator for Taffy cache tests under `tests/taffy_cache/`.
//!
//! This harness intentionally remains separate from the large `misc_tests` binary because
//! `src/layout/taffy_integration.rs` snapshots environment-driven cache limit overrides on first
//! use. Keeping these tests in their own process avoids order-dependent failures when other tests
//! touch flex/grid layout earlier in the run.

mod taffy_cache;

