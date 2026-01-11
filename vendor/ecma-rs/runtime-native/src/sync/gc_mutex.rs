use crate::threading;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

/// A `parking_lot::Mutex` wrapper that is aware of the runtime's stop-the-world
/// safepoint mechanism.
///
/// `lock()` uses a fast uncontended `try_lock()` path. If the lock is contended,
/// it temporarily enters a GC-safe ("native") region while waiting so the
/// safepoint coordinator does not wait for this thread to reach a cooperative
/// safepoint poll.
///
/// Once the mutex is acquired, the GC-safe region is exited immediately and the
/// returned guard is usable as normal mutator code.
pub struct GcAwareMutex<T> {
  inner: Mutex<T>,
}

impl<T> GcAwareMutex<T> {
  pub const fn new(value: T) -> Self {
    Self {
      inner: Mutex::new(value),
    }
  }

  pub fn into_inner(self) -> T {
    self.inner.into_inner()
  }

  pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
    self.inner.try_lock()
  }

  pub fn lock(&self) -> MutexGuard<'_, T> {
    if let Some(g) = self.inner.try_lock() {
      return g;
    }

    loop {
      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.lock();

      // If a stop-the-world is active, do not resume mutator execution while
      // holding the lock: release and retry after the world is resumed.
      if threading::safepoint::current_epoch() & 1 == 1 {
        drop(guard);
        drop(gc_safe);
        continue;
      }

      drop(gc_safe);
      return guard;
    }
  }
}

impl<T: Default> Default for GcAwareMutex<T> {
  fn default() -> Self {
    Self::new(T::default())
  }
}

