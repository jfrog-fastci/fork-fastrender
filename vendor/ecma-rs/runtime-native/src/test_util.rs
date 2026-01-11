//! Test-only helpers for working with the global `runtime-native` async runtime.
//!
//! `runtime-native` is intentionally implemented as a process-wide singleton. Rust test binaries,
//! however, run tests in parallel threads by default (see `RUST_TEST_THREADS`). This module
//! provides utilities to make integration tests deterministic without forcing
//! `RUST_TEST_THREADS=1`.
//!
//! Note: Integration tests (`runtime-native/tests/*.rs`) build the library as a normal dependency
//! (without `cfg(test)`), so these helpers are kept available in non-test builds as well. They are
//! **not** considered stable API.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::time::Duration;

use crate::async_rt;
use crate::time;

static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Reset the global runtime singleton to a clean, idle state.
///
/// This intentionally does *not* tear down any background threads (if/when they are introduced);
/// it only clears per-process queues and registrations so each test starts from a blank slate.
pub fn reset_runtime_state() {
  async_rt::clear_state_for_tests();
  crate::exports::clear_web_timers_for_tests();
  crate::roots::global_root_registry().clear_for_tests();
  time::debug_clear_state_for_tests();
  crate::async_runtime::reset_for_tests();
}

/// A per-test guard that serializes access to the global runtime singleton.
///
/// Create one at the top of each test:
/// ```no_run
/// # use runtime_native::test_util::TestRuntimeGuard;
/// let _rt = TestRuntimeGuard::new();
/// ```
pub struct TestRuntimeGuard {
  _lock: parking_lot::MutexGuard<'static, ()>,
}

impl TestRuntimeGuard {
  pub fn new() -> Self {
    let lock = TEST_MUTEX.lock();
    reset_runtime_state();
    Self { _lock: lock }
  }
}

impl Drop for TestRuntimeGuard {
  fn drop(&mut self) {
    // Keep tests isolated even if they didn't drain their own queues.
    reset_runtime_state();
  }
}

/// Run a closure with an acquired [`TestRuntimeGuard`].
pub fn with_test_runtime<T>(f: impl FnOnce() -> T) -> T {
  let _guard = TestRuntimeGuard::new();
  f()
}

// --- Scheduling helpers used by integration tests ----------------------------------------------

pub fn enqueue_microtask(func: async_rt::TaskFn, data: *mut u8) {
  async_rt::enqueue_microtask(func, data);
}

pub fn enqueue_macrotask(func: async_rt::TaskFn, data: *mut u8) {
  async_rt::enqueue_macrotask(func, data);
}

pub fn schedule_timer(delay: Duration, func: async_rt::TaskFn, data: *mut u8) -> async_rt::TimerId {
  async_rt::global().schedule_timer_in(delay, async_rt::Task::new(func, data))
}

pub fn set_microtask_checkpoint_end_hook(hook: Option<Box<dyn FnMut() + Send + 'static>>) {
  crate::async_runtime::set_microtask_checkpoint_end_hook(hook);
}
