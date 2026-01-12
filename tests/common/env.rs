//! Environment-variable helpers for integration tests.
//!
//! During test-harness consolidation, some suites refer to environment helpers as
//! `crate::common::env::*` while others use `crate::common::global_state::*`.
//! Keep both module paths available by re-exporting the canonical definitions.

pub(crate) use super::global_state::{
  global_test_lock, with_env_vars, with_global_lock, EnvVarGuard, EnvVarsGuard, GlobalTestLockGuard,
  ScopedEnv, StageListenerGuard,
};
