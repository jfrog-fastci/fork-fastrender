use std::sync::Arc;

/// GC root handle.
///
/// The eventual runtime-native GC will need to treat any runtime-held pointers
/// (e.g. timer callbacks, epoll watcher callbacks) as roots. This type provides
/// an ownership model for those roots without tying the async runtime core to a
/// specific GC implementation yet.
#[derive(Clone)]
pub struct Root {
  inner: Arc<RootInner>,
}

struct RootInner {
  slot: *mut *mut u8,
  handle: u32,
}

// Safety: `RootInner` is an opaque pointer used for bookkeeping only. The
// eventual GC's root registration/unregistration must be thread-safe.
unsafe impl Send for RootInner {}
unsafe impl Sync for RootInner {}

impl Root {
  /// Register `ptr` as a GC root.
  ///
  /// # Safety
  /// `ptr` must be a valid pointer to a GC-managed object.
  pub unsafe fn new_unchecked(ptr: *mut u8) -> Self {
    let slot = Box::into_raw(Box::new(ptr));
    let handle = crate::roots::global_root_registry().register_root_slot(slot);
    Self {
      inner: Arc::new(RootInner { slot, handle }),
    }
  }

  #[allow(dead_code)]
  pub fn ptr(&self) -> *mut u8 {
    // Safety: `slot` is owned by `RootInner` and freed only after it is removed from the global
    // root set.
    unsafe { *self.inner.slot }
  }
}

impl Drop for RootInner {
  fn drop(&mut self) {
    crate::roots::global_root_registry().unregister(self.handle);
    // Safety: `slot` was allocated from `Box::into_raw` in `new_unchecked`.
    unsafe {
      drop(Box::from_raw(self.slot));
    }
  }
}
