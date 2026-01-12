//! Shared helpers for integration tests.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) mod accessibility;
pub(crate) mod net;
pub(crate) mod rayon_test_util;

/// Serialises tests that mutate process-wide state (environment variables, stage listeners, etc).
pub(crate) fn global_test_lock() -> MutexGuard<'static, ()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK
    .get_or_init(|| Mutex::new(()))
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  pub(crate) fn set(key: &'static str, value: impl Into<OsString>) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value.into());
    Self { key, previous }
  }

  #[allow(dead_code)]
  pub(crate) fn unset(key: &'static str) -> Self {
    let previous = std::env::var_os(key);
    std::env::remove_var(key);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}
