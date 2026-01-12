//! GC rooting helpers for async runtime tasks.
//!
//! The async runtime stores task callbacks (and their `data` pointers) in Rust-owned queues. Those
//! queues are not visible to stackmap-based GC scanning, so any GC-managed `data` pointers must be
//! rooted explicitly.

use std::sync::Arc;

use crate::gc::HandleId;

/// GC root handle for async-runtime owned pointers.
///
/// This is used to keep coroutine/promise objects alive while they are referenced by host-owned
/// queues (microtasks, timers, reactor watchers, etc).
///
/// The root is implemented via the global [`crate::roots::PersistentHandleTable`]:
/// - the queued work stores a stable [`HandleId`] (convertible to/from `u64`),
/// - the handle table is traced as part of the GC root set,
/// - and the GC may update the pointed-to value during relocation/compaction under stop-the-world.
#[derive(Clone)]
pub struct Root {
  inner: Arc<RootInner>,
}

struct RootInner {
  id: HandleId,
}

// Safety: `RootInner` contains a stable handle id only. The underlying handle table provides the
// synchronization for alloc/free/get operations.
unsafe impl Send for RootInner {}
unsafe impl Sync for RootInner {}

impl Root {
  /// Register `ptr` as a GC root.
  ///
  /// # Safety
  /// `ptr` is stored as an **opaque address** in the process-global persistent handle table.
  ///
  /// If `ptr` refers to a GC-managed object, it must be the GC **object base pointer** (start of
  /// `ObjHeader`), not an interior pointer into an object payload.
  ///
  /// Pointers that do not point into the GC heap are ignored by GC tracing (they remain valid as
  /// stable handles, but do not keep any GC object alive).
  ///
  /// ## Moving-GC safety
  /// When the current thread is registered with the runtime thread registry, this function is
  /// moving-GC safe even for **movable** objects: it temporarily roots `ptr` in the thread's shadow
  /// stack and only reads the pointer value *after* acquiring the persistent handle table lock,
  /// avoiding a TOCTOU race under lock contention.
  ///
  /// If the current thread is **unregistered**, this falls back to storing `ptr` directly and
  /// provides **no moving-GC safety guarantees**. In that case, this is only safe to use with:
  /// - pointers that are not GC-managed, or
  /// - GC-managed pointers that are known to be stable for the duration of the call (e.g. pinned
  ///   objects), or
  /// - situations where the embedder can guarantee no GC can run concurrently.
  ///
  /// When a `GcHandle` (pointer-to-slot) is already available, prefer
  /// [`Root::new_from_slot_unchecked`] to avoid temporarily pushing a shadow-stack root.
  pub unsafe fn new_unchecked(ptr: *mut u8) -> Self {
    let id = crate::roots::global_persistent_handle_table().alloc_movable(ptr);
    Self {
      inner: Arc::new(RootInner { id }),
    }
  }

  /// Like [`Root::new_unchecked`], but reads the pointer value from an addressable slot after
  /// acquiring the persistent handle table lock.
  ///
  /// This is intended for moving-GC-safe runtime entrypoints that receive GC-managed pointers as
  /// `GcHandle` (pointer-to-slot) handles.
  ///
  /// # Safety
  /// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot that contains a
  /// GC-managed object base pointer.
  pub unsafe fn new_from_slot_unchecked(slot: *mut *mut u8) -> Self {
    let id = crate::roots::global_persistent_handle_table().alloc_from_slot(slot);
    Self {
      inner: Arc::new(RootInner { id }),
    }
  }

  pub fn id(&self) -> HandleId {
    self.inner.id
  }

  /// Resolve the current pointer for this root.
  ///
  /// If the handle was freed unexpectedly, this aborts: the async runtime must not call back into
  /// generated code with a stale GC pointer.
  pub fn ptr(&self) -> *mut u8 {
    crate::roots::global_persistent_handle_table()
      .get(self.inner.id)
      .unwrap_or_else(|| std::process::abort())
  }
}

impl Drop for RootInner {
  fn drop(&mut self) {
    let _ = crate::roots::global_persistent_handle_table().free(self.id);
  }
}
