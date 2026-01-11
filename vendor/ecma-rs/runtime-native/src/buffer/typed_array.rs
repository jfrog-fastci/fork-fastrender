use core::ptr::NonNull;

use super::array_buffer::{ArrayBuffer, ArrayBufferError, PinnedArrayBuffer};

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
  buffer: NonNull<ArrayBuffer>,
  byte_offset: usize,
  length: usize,
}

impl Uint8Array {
  pub fn view(
    buffer: &ArrayBuffer,
    byte_offset: usize,
    length: usize,
  ) -> Result<Self, TypedArrayError> {
    let buffer_byte_len = buffer.byte_len();
    let end = byte_offset
      .checked_add(length)
      .ok_or(TypedArrayError::Range)?;
    if end > buffer_byte_len {
      return Err(TypedArrayError::Range);
    }
    if buffer.is_detached() {
      return Err(TypedArrayError::Buffer(ArrayBufferError::Detached));
    }
    Ok(Self {
      buffer: NonNull::from(buffer),
      byte_offset,
      length,
    })
  }

  #[inline]
  pub fn is_detached(&self) -> bool {
    // SAFETY: `buffer` is a GC-managed edge. In the real runtime, `buffer` is traced and kept alive.
    let buffer = unsafe { self.buffer.as_ref() };
    buffer.is_detached()
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

    // SAFETY: `buffer` is traced/kept alive by the GC in the real runtime.
    let buffer = unsafe { self.buffer.as_ref() };
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
    // SAFETY: this is a GC-managed edge. In the real runtime, `buffer` is traced and kept alive.
    let buffer = unsafe { self.buffer.as_ref() };
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

  /// Pin this view's backing store and return a stable pointer/length pair.
  pub fn pin(&self) -> Result<PinnedUint8Array, TypedArrayError> {
    if self.is_detached() {
      return Err(TypedArrayError::Buffer(ArrayBufferError::Detached));
    }

    // SAFETY: GC-managed edge (see above).
    let buffer = unsafe { self.buffer.as_ref() };
    let pinned = buffer.pin()?;

    let end = self
      .byte_offset
      .checked_add(self.length)
      .ok_or(TypedArrayError::Range)?;
    if end > pinned.len() {
      return Err(TypedArrayError::Range);
    }

    Ok(PinnedUint8Array {
      pinned,
      start: self.byte_offset,
      len: self.length,
    })
  }
}

/// A pinned `Uint8Array` view.
#[derive(Debug)]
pub struct PinnedUint8Array {
  pinned: PinnedArrayBuffer,
  start: usize,
  len: usize,
}

impl PinnedUint8Array {
  #[inline]
  pub fn as_ptr(&self) -> *mut u8 {
    // SAFETY: `start` was validated on construction.
    unsafe { self.pinned.as_ptr().add(self.start) }
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len
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

