//! Shared helpers for integration tests.

pub(crate) mod accessibility;
pub(crate) mod env;
pub(crate) mod global_state;
pub(crate) mod libtest;
pub(crate) mod locks;
pub(crate) mod media_assets;
pub(crate) mod net;
pub(crate) mod rayon;
// Compatibility shim (some older tests still reference `crate::common::rayon_test_util`).
pub(crate) mod rayon_test_util;
pub(crate) mod stack;

#[cfg(windows)]
pub(crate) mod windows_sandbox;

pub(crate) use global_state::{
  global_test_lock, with_current_dir, with_env_vars, with_global_lock, CurrentDirGuard, EnvVarGuard,
  EnvVarsGuard, GlobalTestLockGuard, ScopedEnv, StageListenerGuard,
};
pub(crate) use net::{net_test_lock, try_bind_localhost};
pub(crate) use rayon::init_rayon_for_tests;
pub(crate) use stack::with_large_stack;
