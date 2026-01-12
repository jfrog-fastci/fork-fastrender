//! Standalone harness for the `crates/` directory allowlist test.
//!
//! The implementation lives under `tests/guards/`, but keeping this as its own integration test
//! crate allows running it quickly via `cargo test --test crates_directory_guard` without
//! compiling the full `tests/integration.rs` harness.

#[path = "guards/crates_directory_guard.rs"]
mod crates_directory_guard;
