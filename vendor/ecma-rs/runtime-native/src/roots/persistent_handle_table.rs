use core::ptr::NonNull;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::gc::{HandleId, HandleTable};

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
  inner: Mutex<HandleTable<u8>>,
}

impl PersistentHandleTable {
  pub fn new() -> Self {
    Self {
      inner: Mutex::new(HandleTable::new()),
    }
  }

  /// Allocates a new persistent handle for `ptr`.
  ///
  /// The returned handle remains a GC root until freed via [`Self::free`].
  pub fn alloc(&self, ptr: *mut u8) -> HandleId {
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::process::abort());
    self.inner.lock().alloc(ptr)
  }

  /// Resolves `id` to the current object pointer, or `None` if the handle is stale/freed.
  pub fn get(&self, id: HandleId) -> Option<*mut u8> {
    self.inner.lock().get(id).map(|p| p.as_ptr())
  }

  /// Frees `id`, removing it from the persistent root set and allowing slot reuse.
  pub fn free(&self, id: HandleId) -> bool {
    self.inner.lock().free(id)
  }

  /// Enumerate all live pointer slots.
  ///
  /// This is intended to be used by the GC while the world is stopped, so it can trace/update
  /// persistent-handle roots in bulk.
  pub(crate) fn for_each_root_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    let mut table = self.inner.lock();
    for (_id, slot) in table.iter_live_mut() {
      f(slot as *mut *mut u8);
    }
  }

  pub(crate) fn clear_for_tests(&self) {
    *self.inner.lock() = HandleTable::new();
  }
}

/// Global persistent handle table.
pub fn global_persistent_handle_table() -> &'static PersistentHandleTable {
  static GLOBAL: Lazy<PersistentHandleTable> = Lazy::new(PersistentHandleTable::new);
  &GLOBAL
}

