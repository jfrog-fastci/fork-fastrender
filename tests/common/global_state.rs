//! Process-global state guards for the consolidated integration test binary.
//!
//! As more tests move into `tests/integration.rs`, they begin sharing process-global state:
//! environment variables, the current directory, and global callbacks like the render stage
//! listener. These helpers provide one place to coordinate that state and keep tests deterministic
//! under `cargo test`'s default parallelism.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialises tests that mutate process-global state (environment variables, current directory,
/// etc.).
pub fn global_test_lock() -> MutexGuard<'static, ()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK
    .get_or_init(|| Mutex::new(()))
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII guard for temporary process environment changes.
///
/// This guard holds [`global_test_lock`] for its entire lifetime and restores the previous values
/// on drop.
#[must_use]
pub struct EnvVarGuard {
  _lock: MutexGuard<'static, ()>,
  saved: Vec<(OsString, Option<OsString>)>,
}

/// Alias used by some tests for readability.
pub type ScopedEnv = EnvVarGuard;

impl EnvVarGuard {
  /// Create an empty environment scope.
  ///
  /// Use [`EnvVarGuard::set_var`] / [`EnvVarGuard::remove_var`] to mutate vars after construction,
  /// or [`EnvVarGuard::set`] / [`EnvVarGuard::remove`] for a builder-style API.
  pub fn new() -> Self {
    Self {
      _lock: global_test_lock(),
      saved: Vec::new(),
    }
  }

  fn save_if_needed(&mut self, key: &OsStr) {
    if self.saved.iter().any(|(saved_key, _)| saved_key == key) {
      return;
    }
    self
      .saved
      .push((key.to_os_string(), std::env::var_os(key)));
  }

  /// Set an environment variable, saving the previous value for later restoration.
  pub fn set_var(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    let key = key.as_ref();
    self.save_if_needed(key);
    std::env::set_var(key, value);
  }

  /// Remove an environment variable, saving the previous value for later restoration.
  pub fn remove_var(&mut self, key: impl AsRef<OsStr>) {
    let key = key.as_ref();
    self.save_if_needed(key);
    std::env::remove_var(key);
  }

  /// Builder-style helper to set an environment variable.
  pub fn set(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
    self.set_var(key, value);
    self
  }

  /// Builder-style helper to remove an environment variable.
  pub fn remove(mut self, key: impl AsRef<OsStr>) -> Self {
    self.remove_var(key);
    self
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    while let Some((key, previous)) = self.saved.pop() {
      match previous {
        Some(value) => std::env::set_var(&key, value),
        None => std::env::remove_var(&key),
      }
    }
  }
}

/// RAII guard for temporary current directory changes.
///
/// This guard holds [`global_test_lock`] for its entire lifetime and restores the previous current
/// directory on drop.
#[must_use]
pub struct CurrentDirGuard {
  _lock: MutexGuard<'static, ()>,
  previous: PathBuf,
}

impl CurrentDirGuard {
  pub fn new(path: impl AsRef<Path>) -> Self {
    let lock = global_test_lock();
    let previous = std::env::current_dir().expect("failed to read current dir");
    std::env::set_current_dir(path.as_ref()).expect("failed to set current dir");
    Self {
      _lock: lock,
      previous,
    }
  }
}

impl Drop for CurrentDirGuard {
  fn drop(&mut self) {
    std::env::set_current_dir(&self.previous).expect("failed to restore current dir");
  }
}

/// RAII guard that installs a global stage listener and restores the previous listener on drop.
///
/// Prefer [`fastrender::render_control::push_stage_listener`] when thread-local observation is
/// sufficient; the global listener is invoked by *all* threads.
#[must_use]
pub struct StageListenerGuard {
  _guard: fastrender::render_control::GlobalStageListenerGuard,
}

impl StageListenerGuard {
  pub fn new(listener: fastrender::render_control::StageListener) -> Self {
    Self {
      _guard: fastrender::render_control::GlobalStageListenerGuard::new(listener),
    }
  }
}
