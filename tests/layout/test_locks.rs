use parking_lot::{Mutex, MutexGuard};

/// Serialize tests that mutate global layout-parallelism debug state.
///
/// The debug counters in `fastrender::layout::engine` are process-global. The Rust test harness runs
/// many layout tests concurrently, so without a shared lock, tests that toggle/reset the counters
/// can race and observe zeroed counters (or poison the test-local guards).
pub(super) fn layout_parallel_debug_lock() -> MutexGuard<'static, ()> {
  static LOCK: Mutex<()> = Mutex::new(());
  LOCK.lock()
}
