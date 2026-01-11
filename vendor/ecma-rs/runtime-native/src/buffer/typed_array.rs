use core::ptr::NonNull;

use super::array_buffer::{ArrayBuffer, ArrayBufferError, PinnedArrayBuffer};
use super::backing_store::BackingStore;
use crate::gc::{ObjHeader, OBJ_HEADER_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedArrayError {
  Buffer(ArrayBufferError),
  /// Invalid view range.
  Range,
}

impl From<ArrayBufferError> for TypedArrayError {
  fn from(value: ArrayBufferError) -> Self {
    Self::Buffer(value)
  }
}

/// GC-managed (movable) header for a JavaScript `Uint8Array`.
#[derive(Debug)]
#[repr(C)]
pub struct Uint8Array {
  /// Object base pointer of the underlying `ArrayBuffer` (start of `ObjHeader`).
  ///
  /// This is a GC-traced edge and must therefore be an **object pointer**, not a payload pointer
  /// (`base + OBJ_HEADER_SIZE`).
  ///
  /// For non-GC uses of `Uint8Array` (e.g. standalone Rust `ArrayBuffer` values), this may be null.
  /// Such views must not be embedded in the GC heap with a trace map that expects `buffer_obj` to
  /// contain a valid object pointer.
  buffer_obj: *mut u8,
  /// Non-GC pointer to the `ArrayBuffer` header.
  ///
  /// When `buffer_obj` is non-null, this pointer is treated as a cached convenience pointer only:
  /// it is **not traced/relocated** by the GC and may become stale if the buffer header moves. In
  /// that case, methods derive the payload pointer from `buffer_obj` instead.
  buffer: NonNull<ArrayBuffer>,
  byte_offset: usize,
  length: usize,
}

impl Uint8Array {
  #[inline]
  unsafe fn buffer_obj_ptr(&self) -> *mut u8 {
    debug_assert!(!self.buffer_obj.is_null());
    let mut obj = self.buffer_obj;
    loop {
      let header = &*(obj as *const ObjHeader);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
        continue;
      }
      return obj;
    }
  }

  #[inline]
  fn buffer(&self) -> &ArrayBuffer {
    if self.buffer_obj.is_null() {
      // Non-GC/standalone view: `buffer` points directly to an `ArrayBuffer` value.
      // SAFETY: `buffer` is a non-null pointer to a live `ArrayBuffer` header.
      return unsafe { self.buffer.as_ref() };
    }

    // GC-managed view: derive the payload pointer from the traced object base pointer.
    // SAFETY: `buffer_obj` is a GC-managed edge. In the real runtime, it is traced and kept alive.
    unsafe { &*(self.buffer_obj_ptr().add(OBJ_HEADER_SIZE) as *const ArrayBuffer) }
  }

  #[inline]
  pub fn view(
    buffer: &ArrayBuffer,
    byte_offset: usize,
    length: usize,
  ) -> Result<Self, TypedArrayError> {
    if buffer.is_detached() {
      return Err(TypedArrayError::Buffer(ArrayBufferError::Detached));
    }
    let buffer_byte_len = buffer.byte_len();
    let end = byte_offset
      .checked_add(length)
      .ok_or(TypedArrayError::Range)?;
    if end > buffer_byte_len {
      return Err(TypedArrayError::Range);
    }
    Ok(Self {
      buffer_obj: core::ptr::null_mut(),
      buffer: NonNull::from(buffer),
      byte_offset,
      length,
    })
  }

  /// Create a view over a GC-managed `ArrayBuffer` object.
  ///
  /// `buffer_obj` must be the **object base pointer** (start of [`ObjHeader`]) for a GC-managed
  /// allocation that contains an [`ArrayBuffer`] payload at `buffer_obj + OBJ_HEADER_SIZE`.
  pub fn view_gc(
    buffer_obj: *mut u8,
    byte_offset: usize,
    length: usize,
  ) -> Result<Self, TypedArrayError> {
    let buffer_obj = NonNull::new(buffer_obj).ok_or(TypedArrayError::Buffer(ArrayBufferError::Detached))?;

    // Follow forwarding pointers defensively: callers should not pass stale nursery pointers, but
    // this makes the helper more robust in debug/test scenarios.
    let mut obj = buffer_obj.as_ptr();
    loop {
      // SAFETY: `obj` is expected to point to the start of a valid GC-managed object.
      let header = unsafe { &*(obj as *const ObjHeader) };
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
        continue;
      }
      break;
    }

    // SAFETY: `obj` points to the base of an `ArrayBuffer` allocation.
    let buffer = unsafe { &*(obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    if buffer.is_detached() {
      return Err(TypedArrayError::Buffer(ArrayBufferError::Detached));
    }

    let buffer_byte_len = buffer.byte_len();
    let end = byte_offset
      .checked_add(length)
      .ok_or(TypedArrayError::Range)?;
    if end > buffer_byte_len {
      return Err(TypedArrayError::Range);
    }

    Ok(Self {
      buffer_obj: obj,
      buffer: NonNull::from(buffer),
      byte_offset,
      length,
    })
  }

  #[inline]
  pub fn is_detached(&self) -> bool {
    self.buffer().is_detached()
  }

  /// `Uint8Array.prototype.length`.
  ///
  /// When the backing buffer is detached this becomes `0`.
  #[inline]
  pub fn length(&self) -> usize {
    if self.is_detached() { 0 } else { self.length }
  }

  /// `Uint8Array.prototype.byteLength`.
  #[inline]
  pub fn byte_length(&self) -> usize {
    self.length()
  }

  /// `Uint8Array.prototype.byteOffset`.
  ///
  /// When the backing buffer is detached this becomes `0`.
  #[inline]
  pub fn byte_offset(&self) -> usize {
    if self.is_detached() { 0 } else { self.byte_offset }
  }

  /// Read an element.
  ///
  /// This is "undefined-like": detached or out-of-bounds reads return `Ok(None)`.
  pub fn get(&self, index: usize) -> Result<Option<u8>, TypedArrayError> {
    if self.is_detached() {
      return Ok(None);
    }
    if index >= self.length {
      return Ok(None);
    }

    let buffer = self.buffer();
    let base_ptr = buffer.data_ptr()?;

    let abs = self
      .byte_offset
      .checked_add(index)
      .ok_or(TypedArrayError::Range)?;
    if abs >= buffer.byte_len() {
      return Ok(None);
    }

    // SAFETY: bounds checked above.
    let byte = unsafe { *base_ptr.add(abs) };
    Ok(Some(byte))
  }

  /// Returns a raw pointer + length for this view (not pinned).
  ///
  /// Callers that need to hold the pointer across async I/O must use [`Self::pin`].
  pub fn as_ptr_range(&self) -> Result<(*mut u8, usize), TypedArrayError> {
    let buffer = self.buffer();
    let base_ptr = buffer.data_ptr()?;

    let end = self
      .byte_offset
      .checked_add(self.length)
      .ok_or(TypedArrayError::Range)?;
    if end > buffer.byte_len() {
      return Err(TypedArrayError::Range);
    }

    // SAFETY: bounds checked above.
    let ptr = unsafe { base_ptr.add(self.byte_offset) };
    Ok((ptr, self.length))
  }

  /// Returns a clone of the underlying backing store handle.
  ///
  /// This is intended for async I/O/FFI subsystems that need to retain/pin the backing allocation
  /// without storing raw pointers to GC-managed `ArrayBuffer`/`TypedArray` headers.
  pub fn backing_store_handle(&self) -> Result<BackingStore, TypedArrayError> {
    self
      .buffer()
      .backing_store_handle()
      .ok_or(TypedArrayError::Buffer(ArrayBufferError::Detached))
  }

  /// Pin this view's backing store and return a stable pointer/length pair.
  pub fn pin(&self) -> Result<PinnedUint8Array, TypedArrayError> {
    self.pin_range(0..self.length)
  }

  /// Pin a subrange of this view and return a stable pointer/length pair.
  ///
  /// `range` is relative to the start of the view (not the underlying buffer).
  pub fn pin_range(&self, range: core::ops::Range<usize>) -> Result<PinnedUint8Array, TypedArrayError> {
    if self.is_detached() {
      return Err(TypedArrayError::Buffer(ArrayBufferError::Detached));
    }
    if range.start > range.end || range.end > self.length {
      return Err(TypedArrayError::Range);
    }

    let abs_start = self
      .byte_offset
      .checked_add(range.start)
      .ok_or(TypedArrayError::Range)?;
    let abs_end = self
      .byte_offset
      .checked_add(range.end)
      .ok_or(TypedArrayError::Range)?;

    let buffer = self.buffer();
    let pinned = buffer.pin_range(abs_start..abs_end)?;

    Ok(PinnedUint8Array {
      pinned,
      start: 0,
      len: abs_end - abs_start,
    })
  }
}

/// A pinned `Uint8Array` view.
#[must_use = "PinnedUint8Array must be kept alive to keep the backing store pinned"]
#[derive(Debug)]
pub struct PinnedUint8Array {
  pinned: PinnedArrayBuffer,
  start: usize,
  len: usize,
}

// SAFETY: `PinnedUint8Array` is an owned view over a `PinnedArrayBuffer` backing store. The guard
// pins the underlying external allocation, and moving the value across threads transfers ownership
// of that pin.
unsafe impl Send for PinnedUint8Array {}

impl PinnedUint8Array {
  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    // SAFETY: `start` was validated on construction.
    unsafe { self.pinned.as_ptr().add(self.start) }
  }

  #[inline]
  pub(crate) fn backing_store(&self) -> &BackingStore {
    self.pinned.backing_store()
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  #[inline]
  pub(crate) fn backing_store_alloc_len(&self) -> usize {
    self.pinned.backing_store_alloc_len()
  }

  #[inline]
  pub(crate) fn backing_store_id(&self) -> usize {
    self.pinned.backing_store_id()
  }

  /// # Safety
  /// The returned slice is valid for as long as this guard is alive.
  #[inline]
  pub unsafe fn as_slice(&self) -> &[u8] {
    core::slice::from_raw_parts(self.as_ptr() as *const u8, self.len)
  }

  /// # Safety
  /// The returned slice is valid for as long as this guard is alive.
  #[inline]
  pub unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
    core::slice::from_raw_parts_mut(self.as_ptr(), self.len)
  }
}

#[cfg(test)]
mod gc_trace_tests {
  use super::*;
  use crate::gc::{GcHeap, RootStack, SimpleRememberedSet, TypeDescriptor};

  // `Uint8Array` contains exactly one GC pointer field: the base pointer to its backing `ArrayBuffer`
  // header.
  static UINT8_ARRAY_PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
  static GC_UINT8_ARRAY_DESC: TypeDescriptor = TypeDescriptor::new(
    OBJ_HEADER_SIZE + core::mem::size_of::<Uint8Array>(),
    &UINT8_ARRAY_PTR_OFFSETS,
  );

  #[test]
  fn minor_gc_relocates_uint8array_buffer_pointer() {
    let mut heap = GcHeap::new();

    let buffer_obj = heap.alloc_array_buffer_young(1).unwrap();
    let array_obj = heap.alloc_young(&GC_UINT8_ARRAY_DESC);
    unsafe {
      (array_obj.add(OBJ_HEADER_SIZE) as *mut Uint8Array).write(Uint8Array::view_gc(buffer_obj, 0, 1).unwrap());
    }

    let mut root = array_obj;
    let mut roots = RootStack::new();
    roots.push(&mut root as *mut *mut u8);
    let mut remembered = SimpleRememberedSet::new();
    heap.collect_minor(&mut roots, &mut remembered);

    assert!(!heap.is_in_nursery(root));

    // SAFETY: `root` is a valid `Uint8Array` object after GC.
    let view = unsafe { &*(root.add(OBJ_HEADER_SIZE) as *const Uint8Array) };
    let buffer_after = view.buffer_obj;
    assert!(!heap.is_in_nursery(buffer_after));
    assert!(
      heap.is_in_immix(buffer_after) || heap.is_in_los(buffer_after),
      "expected relocated buffer in old/LOS"
    );

    // Ensure the view can still access the bytes after relocation.
    let (ptr, len) = view.as_ptr_range().unwrap();
    assert_eq!(len, 1);
    unsafe {
      ptr.write(123);
    }
    assert_eq!(view.get(0).unwrap(), Some(123));

    // Drop the backing store handle so the test doesn't leak external bytes.
    let buffer_payload = unsafe { &mut *(buffer_after.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer) };
    buffer_payload.finalize();
  }
}

// SAFETY: `PinnedUint8Array` owns a `PinnedArrayBuffer`, which pins the underlying backing store
// allocation for the lifetime of the guard. The returned pointer is therefore stable and valid for
// `len` bytes until the guard is dropped.
unsafe impl runtime_io_uring::IoBuf for PinnedUint8Array {
  fn stable_ptr(&self) -> NonNull<u8> {
    NonNull::new(self.as_ptr()).expect("PinnedUint8Array pointer must not be null")
  }

  fn len(&self) -> usize {
    self.len
  }
}

unsafe impl runtime_io_uring::IoBufMut for PinnedUint8Array {
  fn stable_mut_ptr(&mut self) -> NonNull<u8> {
    NonNull::new(self.as_ptr()).expect("PinnedUint8Array pointer must not be null")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::buffer::BorrowError;

  #[test]
  fn uint8_array_rejects_safe_access_while_io_borrowed() {
    let mut buf = ArrayBuffer::new_zeroed(4).unwrap();
    let view = Uint8Array::view(&buf, 0, 4).unwrap();

    {
      let _read = buf.try_borrow_io_read().unwrap();
      assert_eq!(
        view.as_ptr_range().unwrap_err(),
        TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed))
      );
      assert_eq!(
        view.get(0).unwrap_err(),
        TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed))
      );
    }

    assert_eq!(view.as_ptr_range().unwrap().1, 4);
    assert_eq!(view.get(0).unwrap(), Some(0));

    {
      let _write = buf.try_borrow_io_write().unwrap();
      assert_eq!(
        view.as_ptr_range().unwrap_err(),
        TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed))
      );
      assert_eq!(
        view.get(0).unwrap_err(),
        TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed))
      );
    }

    assert_eq!(view.as_ptr_range().unwrap().1, 4);
    assert_eq!(view.get(0).unwrap(), Some(0));

    // Sanity: pinning through a view blocks detach until the pin guard is dropped.
    let pinned = view.pin().unwrap();
    assert_eq!(buf.detach().unwrap_err(), ArrayBufferError::Pinned);
    drop(pinned);
    buf.detach().unwrap();
    assert!(buf.is_detached());
  }
}
