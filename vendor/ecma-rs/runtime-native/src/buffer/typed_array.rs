use core::ptr::NonNull;

use super::array_buffer::{ArrayBuffer, ArrayBufferError};

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
  pub fn length(&self) -> usize {
    self.length
  }

  #[inline]
  pub fn byte_offset(&self) -> usize {
    self.byte_offset
  }

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
}
