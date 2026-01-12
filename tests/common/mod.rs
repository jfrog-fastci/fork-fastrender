//! Shared helpers for integration tests.

pub(crate) mod accessibility;
pub(crate) mod global_state;
pub(crate) mod net;
pub(crate) mod rayon_test_util;

pub(crate) use global_state::{global_test_lock, EnvVarGuard};
