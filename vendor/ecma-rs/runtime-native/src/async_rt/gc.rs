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
  /// `ptr` must be a valid pointer to a GC-managed object.
  pub unsafe fn new_unchecked(ptr: *mut u8) -> Self {
    let id = crate::roots::global_persistent_handle_table().alloc(ptr);
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
