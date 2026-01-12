use parking_lot::{Mutex, MutexGuard};

/// Serialize tests that mutate global layout-parallelism debug state.
///
/// Layout debug counters are enabled per-test, but these tests also enable layout fan-out and can
/// create dedicated thread pools with large stacks. Serializing them keeps peak resource usage
/// lower and avoids flakiness from contention when the full suite runs with high `--test-threads`.
pub(super) fn layout_parallel_debug_lock() -> MutexGuard<'static, ()> {
  static LOCK: Mutex<()> = Mutex::new(());
  LOCK.lock()
}

/// Serialize tests that rely on layout profiling counters.
///
/// The layout/float profile counters are global atomics. Even though profiling can be enabled
/// per-thread via runtime toggles, calls to `reset_layout_profile()` reset the shared counters.
/// Serializing profiling-enabled tests keeps counter-based assertions deterministic under high
/// `--test-threads`.
pub(super) fn layout_profile_lock() -> MutexGuard<'static, ()> {
  static LOCK: Mutex<()> = Mutex::new(());
  LOCK.lock()
}
