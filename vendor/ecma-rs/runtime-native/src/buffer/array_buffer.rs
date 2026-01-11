use super::backing_store::{
  global_backing_store_allocator, BackingStore, BackingStoreAllocError, BackingStoreAllocator,
  BackingStorePinError, BorrowError, BorrowGuardRead, BorrowGuardWrite, PinnedBackingStore,
};
use crate::gc::{GcHeap, ObjHeader, RememberedSet, RootSet, TypeDescriptor, OBJ_HEADER_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayBufferError {
  Alloc(BackingStoreAllocError),
  Borrow(BorrowError),
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

impl From<BorrowError> for ArrayBufferError {
  fn from(value: BorrowError) -> Self {
    Self::Borrow(value)
  }
}

/// GC-managed (movable) header for a JavaScript `ArrayBuffer`.
///
/// The header itself is expected to be allocated in the GC heap (and may move); the backing store
/// lives outside the GC heap in a stable, non-moving allocation managed by [`BackingStoreAllocator`].
///
/// The backing store is an independently-owned, reference-counted object. This allows host pin
/// guards to keep the allocation alive even if the owning `ArrayBuffer` header becomes unreachable
/// and is finalized by the GC.
#[derive(Debug)]
#[repr(C)]
pub struct ArrayBuffer {
  byte_len: usize,
  backing_store: Option<BackingStore>,

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
      backing_store: Some(store),
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
      backing_store: Some(store),
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
      backing_store: Some(store),
      flags: 0,
    })
  }

  #[inline]
  pub fn byte_len(&self) -> usize {
    self.byte_len
  }

  #[inline]
  pub fn is_detached(&self) -> bool {
    self.backing_store.is_none()
  }

  /// Current backing store pin count.
  #[inline]
  pub fn pin_count(&self) -> u32 {
    self
      .backing_store
      .as_ref()
      .map_or(0, BackingStore::pin_count)
  }

  #[inline]
  pub fn data_ptr(&self) -> Result<*mut u8, ArrayBufferError> {
    let Some(store) = self.backing_store.as_ref() else {
      return Err(ArrayBufferError::Detached);
    };
    if store.is_io_borrowed() {
      return Err(ArrayBufferError::Borrow(BorrowError::Borrowed));
    }
    Ok(store.as_ptr())
  }

  /// Returns a clone of the underlying backing store handle, if the buffer is not detached.
  ///
  /// This is intended for host subsystems (async I/O, FFI) that must keep the allocation alive
  /// independently of the GC-managed `ArrayBuffer` header.
  #[inline]
  pub fn backing_store_handle(&self) -> Option<BackingStore> {
    self.backing_store.clone()
  }

  #[inline]
  pub fn is_io_borrowed(&self) -> bool {
    self
      .backing_store
      .as_ref()
      .map_or(false, BackingStore::is_io_borrowed)
  }

  pub fn try_borrow_io_read(&self) -> Result<BorrowGuardRead, ArrayBufferError> {
    let Some(store) = self.backing_store.as_ref() else {
      return Err(ArrayBufferError::Detached);
    };
    Ok(store.try_borrow_io_read()?)
  }

  pub fn try_borrow_io_write(&self) -> Result<BorrowGuardWrite, ArrayBufferError> {
    let Some(store) = self.backing_store.as_ref() else {
      return Err(ArrayBufferError::Detached);
    };
    Ok(store.try_borrow_io_write()?)
  }

  /// Temporarily borrow the backing bytes as an immutable slice.
  ///
  /// The callback is generic over the slice lifetime (`for<'a>`), so callers cannot return the
  /// `&[u8]` and hold it beyond the call.
  ///
  /// The backing store is also scoped-borrowed for the duration of the callback, so async I/O
  /// borrows cannot start while the safe slice reference is live.
  ///
  /// ```compile_fail
  /// # use runtime_native::ArrayBuffer;
  /// let buf = ArrayBuffer::new_zeroed(1).unwrap();
  /// let _leaked: &[u8] = buf.try_with_slice(|s| s).unwrap();
  /// ```
  pub fn try_with_slice<R>(&self, f: impl for<'a> FnOnce(&'a [u8]) -> R) -> Result<R, ArrayBufferError> {
    let Some(store) = self.backing_store.as_ref() else {
      return Err(ArrayBufferError::Detached);
    };
    Ok(store.try_with_slice(f)?)
  }

  /// Temporarily borrow the backing bytes as a mutable slice.
  ///
  /// Like [`Self::try_with_slice`], the callback is generic over the slice lifetime so the
  /// `&mut [u8]` cannot escape.
  ///
  /// Like [`Self::try_with_slice`], this also blocks async I/O borrows for the duration of the
  /// callback.
  ///
  /// This additionally requires the backing store to be uniquely owned (no other [`BackingStore`]
  /// handles exist). If the backing store is shared, this returns `BorrowError::NotUnique`.
  ///
  /// ```compile_fail
  /// # use runtime_native::ArrayBuffer;
  /// let mut buf = ArrayBuffer::new_zeroed(1).unwrap();
  /// let _leaked: &mut [u8] = buf.try_with_slice_mut(|s| s).unwrap();
  /// ```
  pub fn try_with_slice_mut<R>(
    &mut self,
    f: impl for<'a> FnOnce(&'a mut [u8]) -> R,
  ) -> Result<R, ArrayBufferError> {
    let Some(store) = self.backing_store.as_mut() else {
      return Err(ArrayBufferError::Detached);
    };
    Ok(store.try_with_slice_mut(f)?)
  }

  /// Pin the backing store bytes and return a stable pointer/length pair.
  ///
  /// While the returned guard is alive, detach/transfer/resize must be rejected.
  pub fn pin(&self) -> Result<PinnedArrayBuffer, ArrayBufferError> {
    self.pin_range(0..self.byte_len)
  }

  /// Pin a subrange of the backing store and return a stable pointer/length pair.
  ///
  /// This is primarily a convenience wrapper around [`BackingStore::pin_range`]. Pinning a subrange
  /// still pins the *whole backing store* for invalidation purposes: detach/transfer/resize must be
  /// rejected while any pin guard exists.
  pub fn pin_range(
    &self,
    range: core::ops::Range<usize>,
  ) -> Result<PinnedArrayBuffer, ArrayBufferError> {
    if self.is_detached() {
      return Err(ArrayBufferError::Detached);
    }

    let store = self
      .backing_store
      .as_ref()
      .expect("detached check above");

    let (pinned, (ptr, len)) = store.pin_range(range).map_err(|err| match err {
      BackingStorePinError::NotAlive => ArrayBufferError::Detached,
      BackingStorePinError::OutOfBounds => ArrayBufferError::Range,
    })?;

    Ok(PinnedArrayBuffer { _pinned: pinned, ptr, len })
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
    if self.is_io_borrowed() {
      return Err(ArrayBufferError::Borrow(BorrowError::Borrowed));
    }
    if start > end || end > self.byte_len {
      return Err(ArrayBufferError::Range);
    }
    let slice_len = end - start;
    let src = self
      .backing_store
      .as_ref()
      .expect("detached check above");

    // Take an exclusive borrow for the duration of the copy. This ensures we don't race with a
    // concurrent async I/O borrow (and kernel access) while performing safe reads of the backing
    // bytes.
    let res = src.try_with_slice(|bytes| {
      let out = Self::new_zeroed_in(alloc, slice_len).map_err(ArrayBufferError::Alloc)?;
      if slice_len == 0 {
        return Ok(out);
      }

      let Some(src_bytes) = bytes.get(start..end) else {
        return Err(ArrayBufferError::Range);
      };
      let dst = out
        .backing_store
        .as_ref()
        .expect("new array buffer has a backing store");

      unsafe {
        core::ptr::copy_nonoverlapping(src_bytes.as_ptr(), dst.as_ptr(), slice_len);
      }
      Ok(out)
    });

    match res {
      Ok(Ok(out)) => Ok(out),
      Ok(Err(err)) => Err(err),
      Err(err) => Err(ArrayBufferError::Borrow(err)),
    }
  }

  /// Detach the backing store.
  ///
  /// Detach is idempotent: detaching an already-detached buffer is a no-op.
  pub fn detach(&mut self) -> Result<(), ArrayBufferError> {
    if self.is_detached() {
      return Ok(());
    }

    let store = self
      .backing_store
      .as_ref()
      .expect("detached check above");
    if store.is_pinned() {
      return Err(ArrayBufferError::Pinned);
    }
    if store.is_io_borrowed() {
      return Err(ArrayBufferError::Borrow(BorrowError::Borrowed));
    }

    drop(self.backing_store.take());
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
        backing_store: None,
        flags: 0,
      });
    }

    let store = self
      .backing_store
      .as_ref()
      .expect("detached check above");
    if store.is_pinned() {
      return Err(ArrayBufferError::Pinned);
    }
    if store.is_io_borrowed() {
      return Err(ArrayBufferError::Borrow(BorrowError::Borrowed));
    }

    let byte_len = self.byte_len;
    let backing_store = self.backing_store.take();

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
    if self.pin_count() != 0 {
      return Err(ArrayBufferError::Pinned);
    }
    if self.is_io_borrowed() {
      return Err(ArrayBufferError::Borrow(BorrowError::Borrowed));
    }
    Err(ArrayBufferError::Unimplemented)
  }

  /// Releases the backing store handle.
  ///
  /// In the moving-GC runtime, this is expected to be called by the GC finalizer once the
  /// `ArrayBuffer` header becomes unreachable.
  ///
  /// The backing store allocation itself is freed only when the last [`BackingStore`] handle is
  /// dropped (e.g. after all host pin guards are released).
  pub fn finalize(&mut self) {
    self.finalize_in(global_backing_store_allocator())
  }

  pub fn finalize_in<A: BackingStoreAllocator + ?Sized>(&mut self, _alloc: &A) {
    drop(self.backing_store.take());
    self.byte_len = 0;
    self.flags = 0;
  }
}

/// A pinned view of an `ArrayBuffer` backing store.
///
/// The returned pointer is stable (non-moving backing store), but the guard also ensures that the
/// buffer cannot be detached/transferred/resized while pinned.
#[must_use = "PinnedArrayBuffer must be kept alive to keep the backing store pinned"]
#[derive(Debug)]
pub struct PinnedArrayBuffer {
  _pinned: PinnedBackingStore,
  ptr: *mut u8,
  len: usize,
}

// Safety: `PinnedArrayBuffer` is an in-flight I/O guard that may be dropped on an async completion
// thread. The guard keeps the backing store alive and stable; the raw pointer is just an address.
// Any concurrent access to the bytes is the caller's responsibility.
unsafe impl Send for PinnedArrayBuffer {}

impl PinnedArrayBuffer {
  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    self.ptr
  }

  #[inline]
  pub(crate) fn backing_store(&self) -> &BackingStore {
    self._pinned.backing_store()
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  #[inline]
  pub(crate) fn backing_store_alloc_len(&self) -> usize {
    self._pinned.alloc_len()
  }

  #[inline]
  pub(crate) fn backing_store_id(&self) -> usize {
    self._pinned.store_id()
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

// -----------------------------------------------------------------------------
// GC integration
// -----------------------------------------------------------------------------

#[repr(C)]
struct GcArrayBuffer {
  header: ObjHeader,
  buf: ArrayBuffer,
}

// IMPORTANT: `ArrayBuffer.backing_store` points to non-GC memory (malloc/mmap) and must never be
// treated as a GC reference. That means this descriptor must contain **no** pointer offsets.
static GC_ARRAY_BUFFER_DESC: TypeDescriptor =
  TypeDescriptor::new(core::mem::size_of::<GcArrayBuffer>(), &[]);

unsafe fn finalize_gc_array_buffer(heap: &mut GcHeap, obj: *mut u8) {
  // SAFETY: `obj` points at a `GcArrayBuffer` allocation.
  let buf = &mut *(obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer);
  buf.finalize_in(heap.backing_store_allocator());
}

impl GcHeap {
  /// Allocate an `ArrayBuffer` header object in the nursery and register a GC finalizer that
  /// releases the backing store when the object becomes unreachable.
  ///
  /// Note: this helper does **not** perform any GC or retry on external allocator OOM. It is kept
  /// for tests and low-level scenarios that can guarantee allocation won't fail.
  #[deprecated(note = "use GcHeap::alloc_array_buffer_young_gc_aware(..) to allow GC+retry on backing-store OOM")]
  pub fn alloc_array_buffer_young(&mut self, len: usize) -> Result<*mut u8, BackingStoreAllocError> {
    let buf = ArrayBuffer::new_zeroed_in(self.backing_store_allocator(), len)?;

    let obj = self.alloc_young(&GC_ARRAY_BUFFER_DESC);
    // SAFETY: `obj` is valid for `GC_ARRAY_BUFFER_DESC.size` bytes; the payload begins after the
    // object header.
    unsafe {
      (obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer).write(buf);
    }

    self.register_finalizer(obj, finalize_gc_array_buffer);
    Ok(obj)
  }

  /// Allocate a GC-managed `ArrayBuffer` header plus external backing store.
  ///
  /// This is a GC-aware allocation routine: it may run a major collection to reclaim unreachable
  /// `ArrayBuffer` backing stores when:
  /// - external bytes exceed configured thresholds, or
  /// - the backing store allocator returns [`BackingStoreAllocError::OutOfMemory`].
  ///
  /// On backing-store OOM, this method runs a major GC and retries exactly once.
  pub fn alloc_array_buffer_young_gc_aware(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
    len: usize,
  ) -> Result<*mut u8, BackingStoreAllocError> {
    let collect_major_or_oom = |heap: &mut GcHeap,
                                roots: &mut dyn RootSet,
                                remembered: &mut dyn RememberedSet|
     -> Result<(), BackingStoreAllocError> {
      heap
        .collect_major(roots, remembered)
        .map_err(|_| BackingStoreAllocError::OutOfMemory)
    };

    // If we're already under external memory pressure (or would exceed the total hard cap with this
    // allocation), attempt to reclaim unreachable `ArrayBuffer` headers before asking the backing
    // store allocator for more bytes.
    //
    // This is important because backing store allocations happen outside the GC heap; without a
    // pre-collection here, the process can hit allocator OOM before the GC has a chance to run any
    // finalizers.
    let projected_total = self
      .estimated_total_bytes_including_external()
      .saturating_add(len);
    if self.external_bytes() > self.config().major_gc_external_bytes_threshold
      || projected_total > self.limits().max_total_bytes
    {
      collect_major_or_oom(self, roots, remembered)?;
    }

    // Enforce the total hard cap deterministically before attempting the external allocation.
    let projected_total = self
      .estimated_total_bytes_including_external()
      .saturating_add(len);
    if projected_total > self.limits().max_total_bytes {
      return Err(BackingStoreAllocError::OutOfMemory);
    }

    // Allocate backing store first so any GC we trigger below doesn't need to worry about rooting a
    // partially-initialized GC header object.
    let buf = match ArrayBuffer::new_zeroed_in(self.backing_store_allocator(), len) {
      Ok(buf) => buf,
      Err(BackingStoreAllocError::OutOfMemory) => {
        collect_major_or_oom(self, roots, remembered)?;
        ArrayBuffer::new_zeroed_in(self.backing_store_allocator(), len)?
      }
      Err(other) => return Err(other),
    };

    let obj = self
      .alloc_object_with_type_desc_gc(&GC_ARRAY_BUFFER_DESC, roots, remembered, None)
      .map_err(|_| BackingStoreAllocError::OutOfMemory)?;

    // SAFETY: `obj` is valid for `GC_ARRAY_BUFFER_DESC.size` bytes; the payload begins after the
    // object header.
    unsafe {
      (obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer).write(buf);
    }

    self.register_finalizer(obj, finalize_gc_array_buffer);
    Ok(obj)
  }
}

#[cfg(test)]
mod gc_trace_tests {
  use super::*;

  #[test]
  fn arraybuffer_backing_store_is_not_a_gc_traced_field() {
    let backing_store_off = (OBJ_HEADER_SIZE + core::mem::offset_of!(ArrayBuffer, backing_store)) as u32;
    assert!(
      !GC_ARRAY_BUFFER_DESC.ptr_offsets().contains(&backing_store_off),
      "ArrayBuffer.backing_store must never be included in GC trace maps"
    );
  }
}

#[cfg(test)]
mod borrow_tests {
  use super::*;

  #[test]
  fn blocks_safe_access_while_io_borrowed() {
    let mut buf = ArrayBuffer::new_zeroed(4).unwrap();

    let read_guard = buf.try_borrow_io_read().unwrap();
    assert_eq!(
      buf.data_ptr().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.try_with_slice(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.try_with_slice_mut(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    drop(read_guard);

    assert_eq!(buf.try_with_slice(|s| s.len()).unwrap(), 4);

    let write_guard = buf.try_borrow_io_write().unwrap();
    assert_eq!(
      buf.data_ptr().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.try_with_slice(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.try_with_slice_mut(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    drop(write_guard);

    assert_eq!(
      buf.try_with_slice_mut(|s| { s[0] = 7; s[0] }).unwrap(),
      7
    );
  }

  #[test]
  fn try_with_slice_mut_requires_unique_backing_store_handle() {
    let mut buf = ArrayBuffer::new_zeroed(4).unwrap();
    let handle = buf.backing_store_handle().unwrap();

    assert_eq!(
      buf.try_with_slice_mut(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::NotUnique)
    );

    drop(handle);

    assert_eq!(
      buf.try_with_slice_mut(|s| { s[0] = 1; s[0] }).unwrap(),
      1
    );
  }

  #[test]
  fn detach_transfer_resize_slice_fail_while_io_borrowed() {
    let mut buf = ArrayBuffer::new_zeroed(4).unwrap();

    let read_guard = buf.try_borrow_io_read().unwrap();
    assert_eq!(
      buf.detach().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.transfer().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.resize(8).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.slice(0, 1).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    drop(read_guard);

    let write_guard = buf.try_borrow_io_write().unwrap();
    assert_eq!(
      buf.detach().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.transfer().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.resize(8).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.slice(0, 1).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    drop(write_guard);
  }
}

#[cfg(test)]
mod pin_tests {
  use super::*;

  #[test]
  fn detach_transfer_resize_fail_while_pinned() {
    let mut buf = ArrayBuffer::new_zeroed(4).unwrap();
    let pinned = buf.pin().unwrap();

    assert_eq!(buf.detach().unwrap_err(), ArrayBufferError::Pinned);
    assert_eq!(buf.transfer().unwrap_err(), ArrayBufferError::Pinned);
    assert_eq!(buf.resize(8).unwrap_err(), ArrayBufferError::Pinned);

    drop(pinned);

    buf.detach().unwrap();
    assert!(buf.is_detached());
  }
}
