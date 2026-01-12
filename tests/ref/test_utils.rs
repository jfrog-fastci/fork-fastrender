use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

pub(crate) fn with_large_stack<F, R>(f: F) -> R
where
  F: FnOnce() -> R + Send + 'static,
  R: Send + 'static,
{
  const STACK_SIZE: usize = 16 * 1024 * 1024;
  let handle = std::thread::Builder::new()
    .stack_size(STACK_SIZE)
    .spawn(f)
    .expect("failed to spawn test thread");
  match handle.join() {
    Ok(result) => result,
    Err(payload) => std::panic::resume_unwind(payload),
  }
}

static ENV_MUTEX: Mutex<()> = Mutex::new(());

pub(crate) struct EnvVarGuard<'a> {
  _lock: MutexGuard<'a, ()>,
  saved: Vec<(&'static str, Option<OsString>)>,
}

impl<'a> EnvVarGuard<'a> {
  pub(crate) fn new(keys: &[&'static str]) -> Self {
    let lock = ENV_MUTEX.lock().expect("env mutex poisoned");
    let saved: Vec<(&'static str, Option<OsString>)> =
      keys.iter().map(|&key| (key, std::env::var_os(key))).collect();
    for &key in keys {
      std::env::remove_var(key);
    }
    Self { _lock: lock, saved }
  }
}

impl Drop for EnvVarGuard<'_> {
  fn drop(&mut self) {
    for (key, value) in self.saved.drain(..) {
      match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
      }
    }
  }
}
