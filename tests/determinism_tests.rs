//! Determinism-focused integration tests.
//!
//! These tests are intentionally consolidated into a dedicated harness so they can share the
//! reference-image comparison utilities (`tests/ref/`) without spawning additional standalone
//! integration-test binaries.

mod r#ref;
mod determinism;
