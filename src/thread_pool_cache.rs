//! Shared helpers for caching Rayon thread pools.
//!
//! FastRender uses a few dedicated Rayon pools (layout fan-out, paint build/rasterize). These pools
//! are cached by requested thread count so callers can vary thread knobs without repeatedly paying
//! pool construction costs.
//!
//! This module centralizes:
//! - cache sizing (`FASTR_THREAD_POOL_CACHE_MAX`)
//! - thread-count clamping (by `system::cpu_budget()` and a hard upper bound)

/// Maximum number of thread pools cached per subsystem (layout / paint).
pub(crate) const THREAD_POOL_CACHE_MAX_ENV: &str = "FASTR_THREAD_POOL_CACHE_MAX";

/// Default maximum number of cached thread pools per subsystem.
pub(crate) const DEFAULT_THREAD_POOL_CACHE_MAX: usize = 4;

/// Hard ceiling on threads for internally constructed Rayon pools.
///
/// This is a guardrail against hostile environment variables or test suites that vary thread-count
/// knobs across runs. It is applied in addition to `system::cpu_budget()`.
pub(crate) const THREAD_POOL_THREADS_HARD_MAX: usize = 256;

pub(crate) fn thread_pool_cache_max() -> usize {
  crate::debug::runtime::runtime_toggles().usize_with_default(
    THREAD_POOL_CACHE_MAX_ENV,
    DEFAULT_THREAD_POOL_CACHE_MAX,
  )
}

pub(crate) fn clamp_thread_count(requested: usize) -> usize {
  requested
    .max(1)
    .min(crate::system::cpu_budget().max(1))
    .min(THREAD_POOL_THREADS_HARD_MAX)
    .max(1)
}

#[cfg(test)]
mod test_lock {
  use parking_lot::{ReentrantMutex, ReentrantMutexGuard};

  // Serialize thread pool cache mutations in unit tests. The test harness runs many tests in
  // parallel and other paint/layout tests may call into the same caches. This lock allows cache
  // sizing/eviction tests to be deterministic.
  static THREAD_POOL_CACHE_TEST_LOCK: ReentrantMutex<()> = ReentrantMutex::new(());

  pub(super) fn lock() -> ReentrantMutexGuard<'static, ()> {
    THREAD_POOL_CACHE_TEST_LOCK.lock()
  }
}

#[cfg(test)]
pub(crate) fn thread_pool_cache_test_lock(
) -> parking_lot::ReentrantMutexGuard<'static, ()> {
  test_lock::lock()
}
