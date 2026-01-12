use core::ptr::NonNull;

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::gc::{HandleId, HandleTable};
use crate::threading::registry;

/// Process-global persistent handle table for relocatable pointers.
///
/// This table is intended for host-owned queues (async tasks, I/O watchers, OS event loop userdata,
/// etc.) that must keep GC-managed objects alive across suspension points without pinning the
/// objects themselves.
///
/// - Callers store the returned [`HandleId`] (convertible to/from `u64`) in their queued state.
/// - The GC treats every live entry as a **root** and may update the stored pointer during
///   relocation/compaction under a stop-the-world (STW) pause.
///
/// # Pointer requirements
/// Pointers stored in this table are treated as **opaque addresses**.
///
/// - If a pointer refers to a GC-managed object, it must be the GC **object base pointer** (start of
///   `ObjHeader`), not an interior pointer into the object payload.
/// - Pointers that do *not* point into the GC heap are ignored by GC tracing (they remain valid as
///   stable handles, but do not keep any GC object alive).
///
/// Interior pointers into GC-managed objects are forbidden: the GC traces roots by interpreting the
/// pointed-to bytes as an `ObjHeader`, which would corrupt memory or crash if given a payload
/// address.
#[derive(Debug, Default)]
pub struct PersistentHandleTable {
  inner: HandleTable<u8>,
  live_count: AtomicUsize,
}

impl PersistentHandleTable {
  pub fn new() -> Self {
    Self {
      inner: HandleTable::new(),
      live_count: AtomicUsize::new(0),
    }
  }

  /// Allocates a new persistent handle for `ptr`.
  ///
  /// The returned handle remains a GC root until freed via [`Self::free`].
  pub fn alloc(&self, ptr: *mut u8) -> HandleId {
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::process::abort());
    let id = self.inner.alloc(ptr);
    self.live_count.fetch_add(1, Ordering::Relaxed);
    id
  }

  /// Like [`Self::alloc`], but reads the pointer value from an addressable slot *after* acquiring
  /// the handle table lock.
  ///
  /// This is the moving-GC-safe variant used by exported C ABI entrypoints that accept GC-managed
  /// pointers as `GcHandle` (pointer-to-slot) handles.
  ///
  /// # Safety
  /// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot.
  pub unsafe fn alloc_from_slot(&self, slot: *mut *mut u8) -> HandleId {
    // Safety: caller contract.
    let id = unsafe { self.inner.alloc_from_slot(slot) };
    self.live_count.fetch_add(1, Ordering::Relaxed);
    id
  }

  /// Moving-GC-safe allocation for a raw pointer value on a registered thread.
  ///
  /// If lock acquisition blocks on contention, the thread may enter a GC-safe ("NativeSafe") region
  /// while waiting. A moving GC can then relocate objects. To avoid capturing a stale pre-relocation
  /// address, this helper temporarily stores `ptr` in the current thread's shadow stack and calls
  /// [`Self::alloc_from_slot`] so the pointer is read only *after* the lock is acquired.
  ///
  /// If the current thread is not registered with the runtime thread registry, this falls back to
  /// [`Self::alloc`]. In that case the caller must ensure `ptr` is either non-GC-managed or otherwise
  /// stable (pinned/non-moving) for the duration of the call.
  pub(crate) fn alloc_movable(&self, ptr: *mut u8) -> HandleId {
    let ts = registry::current_thread_state_ptr();
    if ts.is_null() {
      return self.alloc(ptr);
    }

    // Safety: `current_thread_state_ptr` returns a valid pointer to the current thread's registered
    // `ThreadState` (it is null only if the thread is unregistered).
    let ts = unsafe { &*ts };

    let scope = crate::gc::shadow_stack::RootScope::new(ts);
    let root = scope.root(ptr);

    // Safety: `root.slot_ptr()` returns a valid, aligned pointer to a writable `*mut u8` slot in the
    // current thread's shadow stack.
    unsafe { self.alloc_from_slot(root.slot_ptr()) }
    // `scope` drops here, truncating the shadow stack entry.
  }

  /// Moving-GC-safe pointer update for a raw pointer value on a registered thread.
  ///
  /// See [`Self::alloc_movable`] for rationale and safety notes.
  pub(crate) fn set_movable(&self, id: HandleId, ptr: *mut u8) -> bool {
    let ts = registry::current_thread_state_ptr();
    if ts.is_null() {
      return self.set(id, ptr);
    }

    // Safety: `current_thread_state_ptr` returns a valid pointer to the current thread's registered
    // `ThreadState` (it is null only if the thread is unregistered).
    let ts = unsafe { &*ts };

    let scope = crate::gc::shadow_stack::RootScope::new(ts);
    let root = scope.root(ptr);

    // Safety: `root.slot_ptr()` returns a valid, aligned pointer to a writable `*mut u8` slot in the
    // current thread's shadow stack.
    unsafe { self.set_from_slot(id, root.slot_ptr()) }
    // `scope` drops here, truncating the shadow stack entry.
  }

  /// Resolves `id` to the current object pointer, or `None` if the handle is stale/freed.
  pub fn get(&self, id: HandleId) -> Option<*mut u8> {
    self.inner.get(id).map(|p| p.as_ptr())
  }

  /// Frees `id`, removing it from the persistent root set and allowing slot reuse.
  pub fn free(&self, id: HandleId) -> bool {
    let freed = self.inner.free(id).is_some();
    if freed {
      let prev = self.live_count.fetch_sub(1, Ordering::Relaxed);
      if prev == 0 {
        // Underflow indicates internal bookkeeping corruption; fail fast to avoid skipping root
        // enumeration in moving collectors.
        std::process::abort();
      }
    }
    freed
  }

  /// Update the pointer stored for `id`.
  ///
  /// Returns `true` if the handle was live and successfully updated.
  pub fn set(&self, id: HandleId, ptr: *mut u8) -> bool {
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::process::abort());
    self.inner.set(id, ptr)
  }

  /// Like [`Self::set`], but reads the pointer value from an addressable slot *after* acquiring the
  /// handle table lock.
  ///
  /// # Safety
  /// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot.
  pub unsafe fn set_from_slot(&self, id: HandleId, slot: *mut *mut u8) -> bool {
    // Safety: caller contract.
    unsafe { self.inner.set_from_slot(id, slot) }
  }

  /// Returns the number of currently-live handles in the table.
  pub fn live_count(&self) -> usize {
    self.live_count.load(Ordering::Acquire)
  }

  /// Enumerate all live pointer slots.
  ///
  /// This is intended to be used by the GC while the world is stopped, so it can trace/update
  /// persistent-handle roots in bulk.
  pub(crate) fn for_each_root_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    self.inner.with_stw_update(|guard| {
      for slot in guard.iter_live_slots_mut() {
        f(slot as *mut *mut u8);
      }
    });
  }

  /// Debug/test helper: run `f` while holding the table's shared/read lock.
  ///
  /// This exists so integration tests can deterministically create contention between:
  /// - a long-held read lock, and
  /// - a thread blocked on `alloc/free/set` (write lock),
  /// and assert that the blocked thread transitions into a GC-safe ("NativeSafe") region.
  ///
  /// This method is **not** considered stable API.
  #[doc(hidden)]
  pub fn debug_with_read_lock_for_tests<R>(&self, f: impl FnOnce() -> R) -> R {
    self.inner.debug_with_read_lock_for_tests(f)
  }

  pub(crate) fn clear_for_tests(&self) {
    self.inner.clear_for_tests();
    self.live_count.store(0, Ordering::Release);
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
  use crate::gc::GcHeap;
  use crate::gc::SimpleRememberedSet;

  #[test]
  fn global_persistent_handle_table_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    global_persistent_handle_table().clear_for_tests();

    // Stop-the-world handshakes can take much longer in debug builds (especially
    // under parallel test execution on multi-agent hosts). Keep release builds
    // strict, but give debug builds enough slack to avoid flaky timeouts.
    const TIMEOUT: Duration = if cfg!(debug_assertions) {
      Duration::from_secs(30)
    } else {
      Duration::from_secs(2)
    };

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
        table.inner.with_stw_update(|_| {
          a_locked_tx.send(()).unwrap();
          a_release_rx.recv().unwrap();
        });

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

  #[test]
  fn alloc_from_slot_reads_pointer_after_lock_acquired() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    global_persistent_handle_table().clear_for_tests();

    const TIMEOUT: Duration = Duration::from_secs(2);

    // Treat pointers as opaque addresses; they do not need to be dereferenceable in this test.
    let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
    let new_value: *mut u8 = 0x2222usize as *mut u8;
    // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
    let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

    std::thread::scope(|scope| {
      // Thread A holds the persistent handle table lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to allocate from a slot while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<HandleId>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let table = global_persistent_handle_table();
        table.inner.with_stw_update(|_| {
          a_locked_tx.send(()).unwrap();
          a_release_rx.recv().unwrap();
        });
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the handle table lock");

      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();

        c_start_rx.recv().unwrap();

        let slot_ptr = slot_ptr as *mut *mut u8;
        // Safety: `slot_ptr` is a valid slot pointer.
        let handle = unsafe { global_persistent_handle_table().alloc_from_slot(slot_ptr) };
        c_done_tx.send(handle).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Start thread C's allocation attempt (it should block on the handle table lock).
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
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

      // Update the slot while thread C is blocked. If `alloc_from_slot` incorrectly read the slot
      // before acquiring the lock, it would still observe the old value.
      slot_value = new_value;

      // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
      a_release_tx.send(()).unwrap();

      let handle = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("handle allocation should complete after lock is released");
      assert_eq!(
        global_persistent_handle_table().get(handle),
        Some(new_value),
        "alloc_from_slot must read the slot after acquiring the lock"
      );
      let _ = global_persistent_handle_table().free(handle);
    });
  }

  #[repr(C)]
  struct Obj {
    header: crate::gc::ObjHeader,
    value: usize,
  }

  static OBJ_DESC: crate::TypeDescriptor =
    crate::TypeDescriptor::new(core::mem::size_of::<Obj>(), &[]);

  #[test]
  fn set_movable_reads_slot_after_lock_acquired_under_gc_safe_lock_contention() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    threading::register_current_thread(ThreadKind::Main);

    // Keep the process-global persistent-handle table empty so minor GC doesn't contend on it while
    // we intentionally hold a lock on a *local* table below.
    assert_eq!(
      global_persistent_handle_table().live_count(),
      0,
      "expected no live global persistent handles after test runtime reset"
    );

    // Allocate a nursery object that is guaranteed to move during minor GC.
    let mut heap = GcHeap::new();
    let obj = heap.alloc_young(&OBJ_DESC);
    unsafe {
      (*(obj as *mut Obj)).value = 0xC0FFEE;
    }

    // Root `obj` in the main thread so we can observe its relocated address after evacuation.
    let ts = threading::registry::current_thread_state().expect("main thread must be registered");
    let scope = crate::gc::shadow_stack::RootScope::new(&ts);
    let rooted_obj = scope.root(obj);

    // Create a local handle table and allocate a stable handle we can update later.
    let table = std::sync::Arc::new(PersistentHandleTable::new());
    let handle = table.alloc(0x1234usize as *mut u8);

    // Stop-the-world handshakes can take much longer in debug builds (especially
    // under parallel test execution on multi-agent hosts). Keep release builds
    // strict, but give debug builds enough slack to avoid flaky timeouts.
    const TIMEOUT: Duration = if cfg!(debug_assertions) {
      Duration::from_secs(30)
    } else {
      Duration::from_secs(2)
    };

    std::thread::scope(|scope_threads| {
      // Thread A holds a shared/read lock on the local persistent handle table.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to store a movable raw GC pointer while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

      let table_a = table.clone();
      scope_threads.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);

        table_a.debug_with_read_lock_for_tests(|| {
          // Mark this thread as GC-safe while holding the lock so stop-the-world coordination can
          // proceed even if the thread is blocked on this test channel.
          let gc_safe = threading::enter_gc_safe_region();
          a_locked_tx.send(()).unwrap();
          a_release_rx.recv().unwrap();
          drop(gc_safe);
        });

        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the local persistent handle table read lock");

      let table_c = table.clone();
      let obj_addr = obj as usize;
      scope_threads.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();

        c_start_rx.recv().unwrap();

        // Safety: `ptr` points to the base of a GC-managed object allocated by `GcHeap::alloc_young`.
        let ptr = obj_addr as *mut u8;
        assert!(table_c.set_movable(handle, ptr));
        c_done_tx.send(()).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Start thread C's store attempt (it should block on the table lock).
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (meaning it is blocked on the GC-aware lock).
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
          panic!(
            "thread C did not enter a GC-safe region while blocked on the persistent handle table lock"
          );
        }
        std::thread::yield_now();
      }

      // Run a moving GC (minor evacuation) while thread C is blocked. This should relocate `obj` and
      // update shadow-stack roots in-place.
      let mut remembered = SimpleRememberedSet::new();
      crate::with_world_stopped(|| {
        heap
          .collect_minor_with_shadow_stacks(&mut remembered)
          .expect("minor GC");
      });

      let relocated = rooted_obj.get();
      assert_ne!(
        relocated as usize, obj_addr,
        "expected the nursery object to be evacuated to a new address during minor GC"
      );
      assert!(
        !heap.is_in_nursery(relocated),
        "expected evacuated object to be out of the nursery"
      );
      unsafe {
        assert_eq!((*(relocated as *const Obj)).value, 0xC0FFEE);
      }

      // Release the read lock so thread C can finish updating the handle.
      a_release_tx.send(()).unwrap();
      c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should finish storing into the persistent handle");

      assert_eq!(
        table.get(handle).unwrap() as usize,
        relocated as usize,
        "handle table must store the relocated pointer, not the stale nursery address"
      );
    });

    assert!(table.free(handle));
    assert_eq!(table.live_count(), 0);

    threading::unregister_current_thread();
  }
}
