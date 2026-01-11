use crate::buffer::{ArrayBuffer, ArrayBufferError, PinnedArrayBuffer, PinnedUint8Array, TypedArrayError, Uint8Array};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoVecError {
  ArrayBuffer(ArrayBufferError),
  TypedArray(TypedArrayError),
  TooManySegments,
  OutOfMemory,
}

impl From<ArrayBufferError> for IoVecError {
  fn from(value: ArrayBufferError) -> Self {
    Self::ArrayBuffer(value)
  }
}

impl From<TypedArrayError> for IoVecError {
  fn from(value: TypedArrayError) -> Self {
    Self::TypedArray(value)
  }
}

/// A `TypedArray`/`ArrayBuffer` range describing one `iovec` entry.
#[derive(Clone, Copy, Debug)]
pub enum IoVecRange<'a> {
  /// A `[offset, offset + len)` range into an [`ArrayBuffer`], where `offset` is relative to the
  /// start of the buffer.
  ArrayBuffer {
    buffer: &'a ArrayBuffer,
    offset: usize,
    len: usize,
  },
  /// A `[offset, offset + len)` range into a [`Uint8Array`], where `offset` is relative to the
  /// start of the view (not the underlying buffer).
  Uint8Array {
    view: &'a Uint8Array,
    offset: usize,
    len: usize,
  },
}

impl<'a> IoVecRange<'a> {
  pub fn array_buffer(buffer: &'a ArrayBuffer, offset: usize, len: usize) -> Result<Self, IoVecError> {
    let end = offset
      .checked_add(len)
      .ok_or(IoVecError::ArrayBuffer(ArrayBufferError::Range))?;
    if end > buffer.byte_len() {
      return Err(IoVecError::ArrayBuffer(ArrayBufferError::Range));
    }
    Ok(Self::ArrayBuffer { buffer, offset, len })
  }

  pub fn whole_array_buffer(buffer: &'a ArrayBuffer) -> Self {
    Self::ArrayBuffer {
      buffer,
      offset: 0,
      len: buffer.byte_len(),
    }
  }

  pub fn uint8_array(view: &'a Uint8Array) -> Self {
    Self::Uint8Array {
      view,
      offset: 0,
      len: view.length(),
    }
  }

  pub fn uint8_array_range(view: &'a Uint8Array, offset: usize, len: usize) -> Result<Self, IoVecError> {
    let end = offset
      .checked_add(len)
      .ok_or(IoVecError::TypedArray(TypedArrayError::Range))?;
    if end > view.length() {
      return Err(IoVecError::TypedArray(TypedArrayError::Range));
    }
    Ok(Self::Uint8Array { view, offset, len })
  }
}

#[derive(Debug)]
#[allow(dead_code)]
enum PinGuard {
  ArrayBuffer(PinnedArrayBuffer),
  Uint8Array(PinnedUint8Array),
}

/// A pinned, stable-address `iovec[]` array.
///
/// This is safe to pass to:
/// - `readv` / `writev`
/// - `sendmsg` / `recvmsg` (when embedded in a stable `msghdr`)
/// - io_uring vectored operations
///
/// because:
/// 1) the `iovec[]` descriptor array is host-owned (`Box<[iovec]>`) and therefore has a stable
///    address for the lifetime of this value.
/// 2) each `iov_base` points into a pinned backing store (see [`ArrayBuffer::pin`]).
#[derive(Debug)]
pub struct PinnedIoVec {
  // NOTE: keep `iovecs` before `pins` so pinned backing stores outlive the `iovec[]` descriptors.
  iovecs: Box<[libc::iovec]>,
  #[allow(dead_code)]
  pins: Box<[PinGuard]>,
}

impl PinnedIoVec {
  pub fn try_from_ranges(ranges: &[IoVecRange<'_>]) -> Result<Self, IoVecError> {
    if ranges.len() > (libc::c_int::MAX as usize) {
      // `writev/readv` take `iovcnt: c_int`.
      return Err(IoVecError::TooManySegments);
    }

    let mut pins: Vec<PinGuard> = Vec::new();
    pins
      .try_reserve_exact(ranges.len())
      .map_err(|_| IoVecError::OutOfMemory)?;

    let mut iovecs: Vec<libc::iovec> = Vec::new();
    iovecs
      .try_reserve_exact(ranges.len())
      .map_err(|_| IoVecError::OutOfMemory)?;

    for range in ranges {
      match *range {
        IoVecRange::ArrayBuffer { buffer, offset, len } => {
          let pinned = buffer.pin()?;
          let end = offset
            .checked_add(len)
            .ok_or(IoVecError::ArrayBuffer(ArrayBufferError::Range))?;
          if end > pinned.len() {
            return Err(IoVecError::ArrayBuffer(ArrayBufferError::Range));
          }

          let base = unsafe { pinned.as_ptr().add(offset) };
          iovecs.push(libc::iovec {
            iov_base: base as *mut libc::c_void,
            iov_len: len,
          });
          pins.push(PinGuard::ArrayBuffer(pinned));
        }
        IoVecRange::Uint8Array { view, offset, len } => {
          let pinned = view.pin()?;
          let end = offset
            .checked_add(len)
            .ok_or(IoVecError::TypedArray(TypedArrayError::Range))?;
          if end > pinned.len() {
            return Err(IoVecError::TypedArray(TypedArrayError::Range));
          }

          let base = unsafe { pinned.as_ptr().add(offset) };
          iovecs.push(libc::iovec {
            iov_base: base as *mut libc::c_void,
            iov_len: len,
          });
          pins.push(PinGuard::Uint8Array(pinned));
        }
      }
    }

    Ok(Self {
      iovecs: iovecs.into_boxed_slice(),
      pins: pins.into_boxed_slice(),
    })
  }

  pub fn len(&self) -> usize {
    self.iovecs.len()
  }

  pub fn is_empty(&self) -> bool {
    self.iovecs.is_empty()
  }

  pub fn as_iovec_ptr(&self) -> *const libc::iovec {
    self.iovecs.as_ptr()
  }

  pub fn as_iovecs(&self) -> &[libc::iovec] {
    &self.iovecs
  }
}

/// Alias used by some APIs to emphasize that this is a list of `iovec` entries.
pub type IoVecList = PinnedIoVec;
