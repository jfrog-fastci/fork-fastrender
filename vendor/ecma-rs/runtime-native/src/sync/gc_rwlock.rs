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

  /// Acquire a shared/read lock without participating in GC-safe region transitions.
  ///
  /// This is intended for **stop-the-world GC coordinator** code that may need to
  /// acquire a `GcAwareRwLock` while the global GC epoch is odd.
  ///
  /// Unlike [`Self::read`], this method:
  /// - does **not** enter a GC-safe region while blocking, and
  /// - does **not** retry/avoid returning while stop-the-world is active.
  ///
  /// Callers must ensure it is safe to block here (typically because the world is
  /// already stopped and no mutator can hold the lock while parked at a safepoint).
  pub fn read_for_gc(&self) -> RwLockReadGuard<'_, T> {
    self.inner.read()
  }

  /// Acquire an exclusive/write lock without participating in GC-safe region transitions.
  ///
  /// See [`Self::read_for_gc`].
  pub fn write_for_gc(&self) -> RwLockWriteGuard<'_, T> {
    self.inner.write()
  }

  pub fn into_inner(self) -> T {
    self.inner.into_inner()
  }

  pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
    self.inner.try_read()
  }

  pub fn read(&self) -> RwLockReadGuard<'_, T> {
    if let Some(g) = self.inner.try_read() {
      // Even if the lock is uncontended, do not allow a registered mutator to proceed while a
      // stop-the-world (STW) epoch is active: the GC coordinator may need to acquire runtime locks
      // under STW, and mutators must not run while the epoch is odd.
      //
      // Unregistered threads (not part of STW coordination) and the coordinator itself are allowed
      // to acquire locks during STW.
      if threading::registry::current_thread_state().is_some()
        && !threading::safepoint::in_stop_the_world()
        && threading::safepoint::current_epoch() & 1 == 1
      {
        drop(g);
        threading::safepoint_poll();
      } else {
        return g;
      }
    }

    // Unregistered threads are not part of stop-the-world coordination. Treat this as a plain
    // lock acquisition so GC coordinator threads (or other external callers) can still acquire
    // GC-aware locks while the world is stopped.
    if threading::registry::current_thread_state().is_none() {
      return self.inner.read();
    }

    // See `GcAwareMutex::lock`: if we're holding handle-stack roots, we must
    // not enter a GC-safe region while waiting on a contended lock.
    let thread = threading::registry::current_thread_state().expect("registered thread");
    if thread.handle_stack_len() != 0 {
      loop {
        if let Some(g) = self.inner.try_read() {
          let epoch = threading::safepoint::current_epoch();
          if epoch & 1 == 1
            && !threading::safepoint::in_stop_the_world()
            && !threading::safepoint::is_stop_the_world_coordinator(epoch)
          {
            drop(g);
            threading::safepoint_poll();
            continue;
          }
          return g;
        }
        threading::safepoint_poll();
        std::hint::spin_loop();
      }
    }

    loop {
      // The stop-the-world coordinator must be able to acquire locks while the world is stopped;
      // otherwise root enumeration can deadlock if a mutator thread is contending on the same lock.
      let epoch = threading::safepoint::current_epoch();
      if threading::safepoint::in_stop_the_world() || threading::safepoint::is_stop_the_world_coordinator(epoch) {
        return self.inner.read();
      }

      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.read();

      // If a stop-the-world is active, do not resume mutator execution while
      // holding the lock: release and retry after the world is resumed.
      let epoch = threading::safepoint::current_epoch();
      if epoch & 1 == 1
        && !threading::safepoint::in_stop_the_world()
        && !threading::safepoint::is_stop_the_world_coordinator(epoch)
      {
        drop(guard);
        threading::safepoint::wait_while_stop_the_world();
        drop(gc_safe);
        continue;
      }

      gc_safe.exit_no_wait();
      let epoch = threading::safepoint::current_epoch();
      if epoch & 1 == 1
        && !threading::safepoint::in_stop_the_world()
        && !threading::safepoint::is_stop_the_world_coordinator(epoch)
      {
        drop(guard);
        threading::safepoint_poll();
        continue;
      }

      return guard;
    }
  }

  pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
    self.inner.try_write()
  }

  pub fn write(&self) -> RwLockWriteGuard<'_, T> {
    if let Some(g) = self.inner.try_write() {
      // See the corresponding `read` fast-path: avoid returning a lock guard to a registered mutator
      // while STW is active.
      if threading::registry::current_thread_state().is_some()
        && !threading::safepoint::in_stop_the_world()
        && threading::safepoint::current_epoch() & 1 == 1
      {
        drop(g);
        threading::safepoint_poll();
      } else {
        return g;
      }
    }

    // Unregistered threads are not part of stop-the-world coordination. Treat this as a plain
    // lock acquisition so GC coordinator threads (or other external callers) can still acquire
    // GC-aware locks while the world is stopped.
    if threading::registry::current_thread_state().is_none() {
      return self.inner.write();
    }

    // See `GcAwareMutex::lock`: if we're holding handle-stack roots, we must
    // not enter a GC-safe region while waiting on a contended lock.
    let thread = threading::registry::current_thread_state().expect("registered thread");
    if thread.handle_stack_len() != 0 {
      loop {
        if let Some(g) = self.inner.try_write() {
          let epoch = threading::safepoint::current_epoch();
          if epoch & 1 == 1
            && !threading::safepoint::in_stop_the_world()
            && !threading::safepoint::is_stop_the_world_coordinator(epoch)
          {
            drop(g);
            threading::safepoint_poll();
            continue;
          }
          return g;
        }
        threading::safepoint_poll();
        std::hint::spin_loop();
      }
    }

    loop {
      // The stop-the-world coordinator must be able to acquire locks while the world is stopped;
      // otherwise root enumeration can deadlock if a mutator thread is contending on the same lock.
      let epoch = threading::safepoint::current_epoch();
      if threading::safepoint::in_stop_the_world() || threading::safepoint::is_stop_the_world_coordinator(epoch) {
        return self.inner.write();
      }

      let gc_safe = threading::enter_gc_safe_region();
      let guard = self.inner.write();

      // If a stop-the-world is active, do not resume mutator execution while
      // holding the lock: release and retry after the world is resumed.
      let epoch = threading::safepoint::current_epoch();
      if epoch & 1 == 1
        && !threading::safepoint::in_stop_the_world()
        && !threading::safepoint::is_stop_the_world_coordinator(epoch)
      {
        drop(guard);
        threading::safepoint::wait_while_stop_the_world();
        drop(gc_safe);
        continue;
      }

      gc_safe.exit_no_wait();
      let epoch = threading::safepoint::current_epoch();
      if epoch & 1 == 1
        && !threading::safepoint::in_stop_the_world()
        && !threading::safepoint::is_stop_the_world_coordinator(epoch)
      {
        drop(guard);
        threading::safepoint_poll();
        continue;
      }

      return guard;
    }
  }
}

impl<T: Default> Default for GcAwareRwLock<T> {
  fn default() -> Self {
    Self::new(T::default())
  }
}
