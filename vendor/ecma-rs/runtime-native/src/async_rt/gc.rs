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
    // Use an internally-owned slot in the global root registry so async/runtime code can store a
    // stable handle without managing per-task slot storage.
    let handle = crate::roots::global_root_registry().pin(ptr);
    Self {
      inner: Arc::new(RootInner { handle }),
    }
  }

  #[allow(dead_code)]
  pub fn ptr(&self) -> *mut u8 {
    crate::roots::global_root_registry()
      .get(self.inner.handle)
      .unwrap_or_else(|| std::process::abort())
  }
}

impl Drop for RootInner {
  fn drop(&mut self) {
    crate::roots::global_root_registry().unregister(self.handle);
  }
}
