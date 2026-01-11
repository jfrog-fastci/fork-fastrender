use core::ptr::NonNull;

use once_cell::sync::Lazy;

use crate::gc::{HandleId, HandleTable};
use crate::sync::GcAwareRwLock;

/// Process-global persistent handle table for GC-managed object pointers.
///
/// This table is intended for host-owned queues (async tasks, I/O watchers, OS event loop userdata,
/// etc.) that must keep GC-managed objects alive across suspension points without pinning the
/// objects themselves.
///
/// - Callers store the returned [`HandleId`] (convertible to/from `u64`) in their queued state.
/// - The GC treats every live entry as a **root** and may update the stored pointer during
///   relocation/compaction under a stop-the-world (STW) pause.
#[derive(Debug, Default)]
pub struct PersistentHandleTable {
  inner: GcAwareRwLock<HandleTable<u8>>,
}

impl PersistentHandleTable {
  pub fn new() -> Self {
    Self {
      inner: GcAwareRwLock::new(HandleTable::new()),
    }
  }

  /// Allocates a new persistent handle for `ptr`.
  ///
  /// The returned handle remains a GC root until freed via [`Self::free`].
  pub fn alloc(&self, ptr: *mut u8) -> HandleId {
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::process::abort());
    self.inner.write().alloc(ptr)
  }

  /// Resolves `id` to the current object pointer, or `None` if the handle is stale/freed.
  pub fn get(&self, id: HandleId) -> Option<*mut u8> {
    self.inner.read().get(id).map(|p| p.as_ptr())
  }

  /// Frees `id`, removing it from the persistent root set and allowing slot reuse.
  pub fn free(&self, id: HandleId) -> bool {
    self.inner.write().free(id).is_some()
  }

  /// Update the pointer stored for `id`.
  ///
  /// Returns `true` if the handle was live and successfully updated.
  pub fn set(&self, id: HandleId, ptr: *mut u8) -> bool {
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::process::abort());
    self.inner.write().set(id, ptr)
  }

  /// Enumerate all live pointer slots.
  ///
  /// This is intended to be used by the GC while the world is stopped, so it can trace/update
  /// persistent-handle roots in bulk.
  pub(crate) fn for_each_root_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    // This is only called while stop-the-world is active. Avoid `GcAwareRwLock::write()`'s contended
    // path here because it intentionally waits for the world to resume before returning a guard.
    //
    // Instead, spin on `try_write()` until we acquire the guard. Under STW there should be no active
    // mutator that can hold the lock indefinitely.
    let table = loop {
      if let Some(g) = self.inner.try_write() {
        break g;
      }
      std::thread::yield_now();
    };
    table.with_stw_update(|guard| {
      for slot in guard.iter_live_slots_mut() {
        f(slot as *mut *mut u8);
      }
    });
  }

  pub(crate) fn clear_for_tests(&self) {
    self.inner.write().clear_for_tests();
  }
}

/// Global persistent handle table.
pub fn global_persistent_handle_table() -> &'static PersistentHandleTable {
  static GLOBAL: Lazy<PersistentHandleTable> = Lazy::new(PersistentHandleTable::new);
  &GLOBAL
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn global_persistent_handle_table_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    global_persistent_handle_table().clear_for_tests();

    const TIMEOUT: Duration = Duration::from_secs(2);

    std::thread::scope(|scope| {
      // Thread A holds the persistent handle table lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to allocate a handle while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<HandleId>();
      let (c_finish_tx, c_finish_rx) = mpsc::channel::<()>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let table = global_persistent_handle_table();
        let guard = table.inner.write();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the handle table lock");

      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();

        c_start_rx.recv().unwrap();

        // Pointers are treated as opaque addresses; they don't need to be dereferenceable here.
        let handle = global_persistent_handle_table().alloc(0x1234usize as *mut u8);
        c_done_tx.send(handle).unwrap();

        c_finish_rx.recv().unwrap();
        let _ = global_persistent_handle_table().free(handle);
        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the lock before starting STW.
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
      let start = Instant::now();
      loop {
        let mut native_safe = false;
        threading::registry::for_each_thread(|t| {
          if t.id() == c_id {
            native_safe = t.is_native_safe();
          }
        });

        if native_safe {
          break;
        }
        if start.elapsed() > TIMEOUT {
          panic!("thread C did not enter a GC-safe region while blocked on the persistent handle table lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      struct ResumeOnDrop;
      impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
          crate::threading::safepoint::rt_gc_resume_world();
        }
      }
      let _resume = ResumeOnDrop;

      // Let thread A release the lock and reach the safepoint.
      a_release_tx.send(()).unwrap();

      assert!(
        crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
        "world failed to stop within timeout; handle table lock contention must not block STW"
      );

      // Root enumeration must be able to lock the global handle table while the world is stopped.
      let (enum_done_tx, enum_done_rx) = mpsc::channel::<()>();
      let watchdog = scope.spawn(move || {
        if enum_done_rx.recv_timeout(TIMEOUT).is_err() {
          crate::threading::safepoint::rt_gc_resume_world();
          panic!("persistent handle table root enumeration deadlocked on the lock");
        }
      });
      global_persistent_handle_table().for_each_root_slot(|_| {});
      let _ = enum_done_tx.send(());
      watchdog.join().unwrap();

      // Resume the world so the contending allocation can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      let handle = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("handle allocation should complete after world is resumed");
      assert_ne!(handle.to_u64(), 0);

      c_finish_tx.send(()).unwrap();
    });
  }
}
