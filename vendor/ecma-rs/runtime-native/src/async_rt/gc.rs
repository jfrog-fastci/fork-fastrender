use std::sync::Arc;

/// Stubbed GC root handle.
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
  ptr: *mut u8,
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
    register_root(ptr);
    Self {
      inner: Arc::new(RootInner { ptr }),
    }
  }

  #[allow(dead_code)]
  pub fn ptr(&self) -> *mut u8 {
    self.inner.ptr
  }
}

impl Drop for RootInner {
  fn drop(&mut self) {
    unregister_root(self.ptr);
  }
}

fn register_root(_ptr: *mut u8) {}

fn unregister_root(_ptr: *mut u8) {}
