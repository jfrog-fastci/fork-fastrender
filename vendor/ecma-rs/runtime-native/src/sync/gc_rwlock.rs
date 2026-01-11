use crate::threading;
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use parking_lot::RwLockWriteGuard;

impl<T> std::fmt::Debug for GcAwareRwLock<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GcAwareRwLock").finish_non_exhaustive()
  }
}

/// A `parking_lot::RwLock` wrapper that is aware of the runtime's stop-the-world
/// safepoint mechanism.
///
/// Like [`super::GcAwareMutex`], contended lock acquisition enters a GC-safe
/// ("native") region while blocked so stop-the-world coordination doesn't wait
/// for this thread to reach a cooperative safepoint poll.
pub struct GcAwareRwLock<T> {
  inner: RwLock<T>,
}

impl<T> GcAwareRwLock<T> {
  pub const fn new(value: T) -> Self {
    Self {
      inner: RwLock::new(value),
    }
  }

  pub fn into_inner(self) -> T {
    self.inner.into_inner()
  }

  pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
    self.inner.try_read()
  }

  pub fn read(&self) -> RwLockReadGuard<'_, T> {
    if let Some(g) = self.inner.try_read() {
      return g;
    }

    loop {
      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.read();

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

  pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
    self.inner.try_write()
  }

  pub fn write(&self) -> RwLockWriteGuard<'_, T> {
    if let Some(g) = self.inner.try_write() {
      return g;
    }

    loop {
      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.write();

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

impl<T: Default> Default for GcAwareRwLock<T> {
  fn default() -> Self {
    Self::new(T::default())
  }
}
