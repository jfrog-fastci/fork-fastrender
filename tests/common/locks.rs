//! Lock helpers for integration tests.
//!
//! The canonical implementation lives in [`crate::common::global_state`], but some migrated tests
//! reference the lock via `crate::common::locks::global_test_lock()`. Keep this module as a thin
//! compatibility shim.

pub(crate) use super::global_state::global_test_lock;
