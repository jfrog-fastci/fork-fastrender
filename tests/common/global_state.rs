//! Process-global state guards for the consolidated integration test binary.
//!
//! When tests were split across many `tests/*.rs` integration binaries, each test suite ran in its
//! own process. As more suites are consolidated into the unified `tests/integration.rs` harness,
//! tests that mutate process-global state (environment variables, stage listeners, etc.) must
//! coordinate to remain deterministic under parallel execution.

use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub type GlobalTestLockGuard = ReentrantMutexGuard<'static, ()>;

/// Serialises tests that mutate process-global state (environment variables, stage listeners, etc.).
pub fn global_test_lock() -> GlobalTestLockGuard {
  // Many integration tests have helper layers that may re-enter the global lock (e.g. a top-level
  // test takes the lock, then constructs a helper that also takes it). Use a re-entrant mutex so
  // these call paths do not deadlock.
  static LOCK: ReentrantMutex<()> = ReentrantMutex::new(());
  LOCK.lock()
}
/// Run `f` while holding the process-global test lock.
pub(crate) fn with_global_lock<R>(f: impl FnOnce() -> R) -> R {
  let _lock = global_test_lock();
  f()
}

/// RAII guard that sets a single environment variable (or removes it) while holding the global
/// test lock.
#[must_use]
pub(crate) struct EnvVarGuard {
  _lock: GlobalTestLockGuard,
  key: OsString,
  previous: Option<OsString>,
}

/// Alias used by some tests for readability.
pub type ScopedEnv = EnvVarGuard;

impl EnvVarGuard {
  fn assert_env_var_allowed(key: &OsStr) {
    if key == OsStr::new("FASTR_USE_BUNDLED_FONTS") {
      panic!(
        "integration tests must not mutate FASTR_USE_BUNDLED_FONTS; configure the renderer with FontConfig::bundled_only() instead"
      );
    }
    if key == OsStr::new("RAYON_NUM_THREADS") {
      panic!(
        "integration tests must not mutate RAYON_NUM_THREADS; rely on FastRender's safe global pool initialisation (via common::init_rayon_for_tests) or disable parallelism via RenderOptions/renderer config instead"
      );
    }
  }

  /// Set `key` to `value` for the lifetime of this guard.
  pub(crate) fn set(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
    let key = key.as_ref();
    Self::assert_env_var_allowed(key);
    let lock = global_test_lock();
    let key = key.to_owned();
    let previous = std::env::var_os(&key);
    std::env::set_var(&key, value);
    Self {
      _lock: lock,
      key,
      previous,
    }
  }

  /// Remove `key` for the lifetime of this guard.
  pub(crate) fn remove(key: impl AsRef<OsStr>) -> Self {
    let key = key.as_ref();
    Self::assert_env_var_allowed(key);
    let lock = global_test_lock();
    let key = key.to_owned();
    let previous = std::env::var_os(&key);
    std::env::remove_var(&key);
    Self {
      _lock: lock,
      key,
      previous,
    }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(&self.key, value),
      None => std::env::remove_var(&self.key),
    }
  }
}

/// RAII guard that sets/removes multiple environment variables while holding the global test lock.
#[must_use]
pub(crate) struct EnvVarsGuard {
  _lock: GlobalTestLockGuard,
  // Restore in reverse order so repeated keys behave intuitively.
  saved: Vec<(OsString, Option<OsString>)>,
}

impl EnvVarsGuard {
  /// Apply `vars` for the lifetime of this guard.
  ///
  /// Each entry is `(key, Some(value))` to set the variable or `(key, None)` to remove it.
  pub(crate) fn new(vars: &[(&str, Option<&str>)]) -> Self {
    let lock = global_test_lock();
    let mut saved = Vec::with_capacity(vars.len());
    for (key, value) in vars {
      EnvVarGuard::assert_env_var_allowed(OsStr::new(key));
      let key_os: OsString = OsString::from(key);
      let prev = std::env::var_os(&key_os);
      saved.push((key_os.clone(), prev));
      match value {
        Some(v) => std::env::set_var(&key_os, v),
        None => std::env::remove_var(&key_os),
      }
    }
    Self { _lock: lock, saved }
  }

  /// Convenience for setting multiple environment variables.
  pub(crate) fn set(vars: &[(&str, &str)]) -> Self {
    // Build a temporary `Vec` rather than requiring `Some(...)` at call-sites.
    let mapped: Vec<(&str, Option<&str>)> = vars.iter().map(|(k, v)| (*k, Some(*v))).collect();
    Self::new(&mapped)
  }

  /// Convenience for removing multiple environment variables.
  pub(crate) fn remove(keys: &[&str]) -> Self {
    let mapped: Vec<(&str, Option<&str>)> = keys.iter().map(|k| (*k, None)).collect();
    Self::new(&mapped)
  }
}

impl Drop for EnvVarsGuard {
  fn drop(&mut self) {
    while let Some((key, prev)) = self.saved.pop() {
      match prev {
        Some(value) => std::env::set_var(&key, value),
        None => std::env::remove_var(&key),
      }
    }
  }
}

/// RAII guard that sets the current working directory for the lifetime of the guard while holding
/// the global test lock.
///
/// This is needed because `std::env::set_current_dir` is process-global in the unified integration
/// test binary. Tests should use this guard (or [`with_current_dir`]) so changes are serialized and
/// automatically reverted.
#[must_use]
pub(crate) struct CurrentDirGuard {
  _lock: GlobalTestLockGuard,
  previous: PathBuf,
}

impl CurrentDirGuard {
  /// Set the current directory to `path` for the lifetime of this guard.
  pub(crate) fn set(path: impl AsRef<Path>) -> std::io::Result<Self> {
    let lock = global_test_lock();
    let previous = std::env::current_dir()?;
    std::env::set_current_dir(path)?;
    Ok(Self { _lock: lock, previous })
  }
}

impl Drop for CurrentDirGuard {
  fn drop(&mut self) {
    let _ = std::env::set_current_dir(&self.previous);
  }
}

/// Run `f` while the current directory is set to `path`.
pub(crate) fn with_current_dir<R>(
  path: impl AsRef<Path>,
  f: impl FnOnce() -> R,
) -> std::io::Result<R> {
  let _guard = CurrentDirGuard::set(path)?;
  Ok(f())
}

/// RAII guard that installs a process-global stage listener and restores the previous listener on
/// drop.
///
/// This is a thin wrapper around [`fastrender::render_control::GlobalStageListenerGuard`].
#[must_use]
pub(crate) struct StageListenerGuard {
  _guard: fastrender::render_control::GlobalStageListenerGuard,
}

impl StageListenerGuard {
  pub(crate) fn new(listener: fastrender::render_control::StageListener) -> Self {
    Self {
      _guard: fastrender::render_control::GlobalStageListenerGuard::new(listener),
    }
  }
}

/// Run `f` while `vars` are set.
pub(crate) fn with_env_vars<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
  let _guard = EnvVarsGuard::set(vars);
  f()
}
