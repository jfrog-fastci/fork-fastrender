use crate::threading;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

impl<T> std::fmt::Debug for GcAwareMutex<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GcAwareMutex").finish_non_exhaustive()
  }
}

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

  /// Lock this mutex without participating in GC-safe region transitions.
  ///
  /// This is intended for **stop-the-world GC coordinator** code that may need to
  /// acquire a `GcAwareMutex` while the global GC epoch is odd.
  ///
  /// Unlike [`Self::lock`], this method:
  /// - does **not** enter a GC-safe region while blocking, and
  /// - does **not** retry/avoid returning while stop-the-world is active.
  ///
  /// Callers must ensure it is safe to block here (typically because the world is
  /// already stopped and no mutator can hold the lock while parked at a safepoint).
  pub fn lock_for_gc(&self) -> MutexGuard<'_, T> {
    self.inner.lock()
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

    // Unregistered threads are not part of the mutator registry and therefore
    // do not participate in stop-the-world coordination. Treat this as a plain
    // mutex acquisition so GC coordinator threads can still lock GC-aware
    // runtime mutexes while the world is stopped.
    //
    // This is important for root enumeration: the coordinator may need to take
    // GC-aware locks (e.g. the global root registry) while `RT_GC_EPOCH` is odd.
    if threading::registry::current_thread_state().is_none() {
      return self.inner.lock();
    }

    loop {
      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.lock();

      // If a stop-the-world is active, do not resume mutator execution while
      // holding the lock: release and retry after the world is resumed.
      if threading::safepoint::current_epoch() & 1 == 1 && !threading::safepoint::in_stop_the_world() {
        drop(guard);
        // Wait for the world to resume *while still in a GC-safe region* so the stop-the-world
        // coordinator doesn't wait for this thread to reach a cooperative safepoint while blocked.
        threading::safepoint::wait_while_stop_the_world();
        drop(gc_safe);
        // `GcSafeGuard` is a no-op for unregistered threads; always block until the world is resumed
        // to avoid a busy-spin loop.
        threading::safepoint::wait_while_stop_the_world();
        continue;
      }

      // Exit the GC-safe region *without blocking* while still holding the mutex guard.
      //
      // `GcSafeGuard::drop` waits for any in-progress stop-the-world request to resume before
      // clearing the NativeSafe flag. If STW begins between the epoch check above and `drop(gc_safe)`,
      // dropping would block while holding the mutex, potentially deadlocking the GC coordinator if it
      // needs this lock for root enumeration.
      gc_safe.exit_no_wait();

      // If a stop-the-world request started concurrently, do not proceed while holding the lock.
      // Release and enter the safepoint, then retry.
      if threading::safepoint::current_epoch() & 1 == 1 && !threading::safepoint::in_stop_the_world() {
        drop(guard);
        threading::safepoint_poll();
        continue;
      }

      return guard;
    }
  }
}

impl<T: Default> Default for GcAwareMutex<T> {
  fn default() -> Self {
    Self::new(T::default())
  }
}
