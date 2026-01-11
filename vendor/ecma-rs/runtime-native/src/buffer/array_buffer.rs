use super::backing_store::{
  global_backing_store_allocator, BackingStore, BackingStoreAllocError, BackingStoreAllocator,
  BackingStorePinError, PinnedBackingStore,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayBufferError {
  Alloc(BackingStoreAllocError),
  /// Invalid slice range.
  Range,
  /// Buffer has been detached/finalized.
  Detached,
  /// Operation rejected because the backing store is pinned (in-flight I/O).
  Pinned,
  /// Operation is not supported by the MVP runtime.
  Unimplemented,
}

impl From<BackingStoreAllocError> for ArrayBufferError {
  fn from(value: BackingStoreAllocError) -> Self {
    Self::Alloc(value)
  }
}

/// GC-managed (movable) header for a JavaScript `ArrayBuffer`.
///
/// The header itself is expected to be allocated in the GC heap (and may move); the backing store
/// lives outside the GC heap in a stable, non-moving allocation managed by [`BackingStoreAllocator`].
#[derive(Debug)]
#[repr(C)]
pub struct ArrayBuffer {
  byte_len: usize,
  backing_store: BackingStore,

  flags: u32,
}

impl ArrayBuffer {
  pub fn new_zeroed(len: usize) -> Result<Self, BackingStoreAllocError> {
    Self::new_zeroed_in(global_backing_store_allocator(), len)
  }

  pub fn new_zeroed_in<A: BackingStoreAllocator + ?Sized>(
    alloc: &A,
    len: usize,
  ) -> Result<Self, BackingStoreAllocError> {
    let store = alloc.alloc_zeroed(len)?;
    Ok(Self {
      byte_len: len,
      backing_store: store,
      flags: 0,
    })
  }

  pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, BackingStoreAllocError> {
    Self::from_bytes_in(global_backing_store_allocator(), bytes)
  }

  pub fn from_bytes_in<A: BackingStoreAllocator + ?Sized>(
    alloc: &A,
    bytes: Vec<u8>,
  ) -> Result<Self, BackingStoreAllocError> {
    let byte_len = bytes.len();
    let store = alloc.adopt_vec(bytes)?;
    Ok(Self {
      byte_len,
      backing_store: store,
      flags: 0,
    })
  }

  pub fn from_boxed_slice(bytes: Box<[u8]>) -> Result<Self, BackingStoreAllocError> {
    Self::from_boxed_slice_in(global_backing_store_allocator(), bytes)
  }

  pub fn from_boxed_slice_in<A: BackingStoreAllocator + ?Sized>(
    alloc: &A,
    bytes: Box<[u8]>,
  ) -> Result<Self, BackingStoreAllocError> {
    let byte_len = bytes.len();
    let store = alloc.adopt_boxed_slice(bytes)?;
    Ok(Self {
      byte_len,
      backing_store: store,
      flags: 0,
    })
  }

  #[inline]
  pub fn byte_len(&self) -> usize {
    self.byte_len
  }

  #[inline]
  pub fn backing_store(&self) -> &BackingStore {
    &self.backing_store
  }

  #[inline]
  pub fn backing_store_mut(&mut self) -> &mut BackingStore {
    &mut self.backing_store
  }

  #[inline]
  pub fn is_detached(&self) -> bool {
    self.backing_store.is_empty()
  }

  #[inline]
  pub fn data_ptr(&self) -> Result<*mut u8, ArrayBufferError> {
    if self.is_detached() {
      return Err(ArrayBufferError::Detached);
    }
    Ok(self.backing_store.as_ptr())
  }

  pub fn slice(&self, start: usize, end: usize) -> Result<Self, ArrayBufferError> {
    self.slice_in(global_backing_store_allocator(), start, end)
  }

  pub fn slice_in<A: BackingStoreAllocator + ?Sized>(
    &self,
    alloc: &A,
    start: usize,
    end: usize,
  ) -> Result<Self, ArrayBufferError> {
    if self.is_detached() {
      return Err(ArrayBufferError::Detached);
    }
    if start > end || end > self.byte_len {
      return Err(ArrayBufferError::Range);
    }
    let slice_len = end - start;
    let out = Self::new_zeroed_in(alloc, slice_len).map_err(ArrayBufferError::Alloc)?;
    if slice_len == 0 {
      return Ok(out);
    }

    unsafe {
      core::ptr::copy_nonoverlapping(
        self.backing_store.as_ptr().add(start),
        out.backing_store.as_ptr(),
        slice_len,
      );
    }
    Ok(out)
  }

  /// Pin the backing store bytes and return a stable pointer/length pair.
  ///
  /// While the returned guard is alive, detach/transfer/resize must be rejected.
  pub fn pin(&self) -> Result<PinnedArrayBuffer, ArrayBufferError> {
    if self.is_detached() {
      return Err(ArrayBufferError::Detached);
    }
    let (pinned, (ptr, len)) = self.backing_store.pin().map_err(|err| match err {
      BackingStorePinError::NotAlive => ArrayBufferError::Detached,
      BackingStorePinError::OutOfBounds => ArrayBufferError::Range,
    })?;
    Ok(PinnedArrayBuffer {
      pinned,
      ptr,
      len,
    })
  }

  /// Detach the backing store.
  ///
  /// Detach is idempotent: detaching an already-detached buffer is a no-op.
  pub fn detach(&mut self) -> Result<(), ArrayBufferError> {
    self.detach_in(global_backing_store_allocator())
  }

  pub fn detach_in<A: BackingStoreAllocator + ?Sized>(
    &mut self,
    _alloc: &A,
  ) -> Result<(), ArrayBufferError> {
    if self.is_detached() {
      return Ok(());
    }
    if self.backing_store.is_pinned() {
      return Err(ArrayBufferError::Pinned);
    }
    // Detach is an eager free of the backing store. Unlike GC finalization, it must not proceed
    // while pinned.
    self
      .backing_store
      .try_detach()
      .map_err(|err| match err {
        super::backing_store::BackingStoreDetachError::Pinned => ArrayBufferError::Pinned,
        super::backing_store::BackingStoreDetachError::NotAlive => ArrayBufferError::Detached,
      })?;
    self.byte_len = 0;
    self.flags = 0;
    Ok(())
  }

  /// Transfer the backing store into a new `ArrayBuffer`, detaching `self`.
  ///
  /// This mirrors structured-clone transfer semantics: existing views over `self` observe a detached
  /// buffer, while the returned `ArrayBuffer` owns the bytes.
  pub fn transfer(&mut self) -> Result<Self, ArrayBufferError> {
    if self.is_detached() {
      return Ok(Self {
        byte_len: 0,
        backing_store: BackingStore::empty(),
        flags: 0,
      });
    }
    if self.backing_store.is_pinned() {
      return Err(ArrayBufferError::Pinned);
    }

    let byte_len = self.byte_len;
    let backing_store = std::mem::replace(&mut self.backing_store, BackingStore::empty());

    self.byte_len = 0;
    self.flags = 0;

    Ok(Self {
      byte_len,
      backing_store,
      flags: 0,
    })
  }

  /// Placeholder for resizable ArrayBuffers.
  ///
  /// Resizable buffers are not supported in MVP, but the method exists so callers cannot
  /// accidentally ignore the pin-count rule once resize is wired up.
  pub fn resize(&mut self, _new_len: usize) -> Result<(), ArrayBufferError> {
    if self.backing_store.is_pinned() {
      return Err(ArrayBufferError::Pinned);
    }
    Err(ArrayBufferError::Unimplemented)
  }

  /// Releases the backing store memory.
  ///
  /// In the moving-GC runtime, this is expected to be called by the GC finalizer once the
  /// `ArrayBuffer` header becomes unreachable.
  pub fn finalize(&mut self) {
    self.finalize_in(global_backing_store_allocator())
  }

  pub fn finalize_in<A: BackingStoreAllocator + ?Sized>(&mut self, alloc: &A) {
    alloc.free(&mut self.backing_store);
    self.byte_len = 0;
    self.flags = 0;
  }
}

/// A pinned view of an `ArrayBuffer` backing store.
///
/// The returned pointer is stable (non-moving backing store), but the guard also ensures that the
/// buffer cannot be detached/transferred/resized while pinned.
#[derive(Debug)]
pub struct PinnedArrayBuffer {
  pinned: PinnedBackingStore,
  ptr: *mut u8,
  len: usize,
}

impl PinnedArrayBuffer {
  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    self.ptr
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// # Safety
  /// The returned slice is valid for as long as this guard is alive.
  #[inline]
  pub unsafe fn as_slice(&self) -> &[u8] {
    core::slice::from_raw_parts(self.ptr as *const u8, self.len)
  }

  /// # Safety
  /// The returned slice is valid for as long as this guard is alive.
  #[inline]
  pub unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
    core::slice::from_raw_parts_mut(self.ptr, self.len)
  }
}
