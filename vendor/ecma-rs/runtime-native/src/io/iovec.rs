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

  pub fn as_iovec_mut_ptr(&mut self) -> *mut libc::iovec {
    self.iovecs.as_mut_ptr()
  }

  pub fn as_iovecs(&self) -> &[libc::iovec] {
    &self.iovecs
  }
}

/// Alias used by some APIs to emphasize that this is a list of `iovec` entries.
pub type IoVecList = PinnedIoVec;

#[cfg(unix)]
/// A pinned, stable-address `msghdr` that owns its `iovec[]` descriptor list.
///
/// This is safe to pass to `sendmsg`/`recvmsg` and io_uring `SendMsg`/`RecvMsg` because all
/// user-provided pointers inside the struct point into:
/// - heap-owned, stable allocations (`Box<msghdr>`, optional `Vec<u8>` for `msg_control` / `msg_name`)
/// - pinned backing stores (via the owned [`PinnedIoVec`])
#[derive(Debug)]
pub struct PinnedMsgHdr {
  hdr: Box<libc::msghdr>,
  iovecs: PinnedIoVec,
  #[allow(dead_code)]
  name: Option<Vec<u8>>,
  #[allow(dead_code)]
  control: Option<Vec<u8>>,
}

#[cfg(unix)]
impl PinnedMsgHdr {
  pub fn new(iovecs: PinnedIoVec) -> Self {
    Self::new_inner(iovecs, None, None)
  }

  pub fn with_control(iovecs: PinnedIoVec, control: Vec<u8>) -> Self {
    Self::new_inner(iovecs, None, Some(control))
  }

  pub fn with_name(iovecs: PinnedIoVec, name: Vec<u8>) -> Self {
    Self::new_inner(iovecs, Some(name), None)
  }

  pub fn with_name_and_control(iovecs: PinnedIoVec, name: Vec<u8>, control: Vec<u8>) -> Self {
    Self::new_inner(iovecs, Some(name), Some(control))
  }

  fn new_inner(mut iovecs: PinnedIoVec, name: Option<Vec<u8>>, control: Option<Vec<u8>>) -> Self {
    let (msg_name, msg_namelen) = match &name {
      None => (core::ptr::null_mut(), 0 as libc::socklen_t),
      Some(buf) => (
        buf.as_ptr() as *mut libc::c_void,
        buf.len() as libc::socklen_t,
      ),
    };

    let (msg_control, msg_controllen) = match &control {
      None => (core::ptr::null_mut(), 0usize),
      Some(buf) => (buf.as_ptr() as *mut libc::c_void, buf.len()),
    };

    let msg_iov = iovecs.as_iovec_mut_ptr();
    let msg_iovlen = iovecs.len();

    let hdr = Box::new(libc::msghdr {
      msg_name,
      msg_namelen,
      msg_iov,
      msg_iovlen,
      msg_control,
      msg_controllen,
      msg_flags: 0,
    });

    Self {
      hdr,
      iovecs,
      name,
      control,
    }
  }

  pub fn iovecs(&self) -> &PinnedIoVec {
    &self.iovecs
  }

  pub fn as_msghdr_ptr(&self) -> *const libc::msghdr {
    &*self.hdr as *const libc::msghdr
  }

  pub fn as_msghdr_mut_ptr(&mut self) -> *mut libc::msghdr {
    &mut *self.hdr as *mut libc::msghdr
  }

  pub fn msg_flags(&self) -> libc::c_int {
    self.hdr.msg_flags
  }

  pub fn name_len(&self) -> usize {
    self.hdr.msg_namelen as usize
  }

  pub fn name(&self) -> Option<&[u8]> {
    let buf = self.name.as_ref()?;
    let len = self.name_len().min(buf.len());
    Some(&buf[..len])
  }

  pub fn control_len(&self) -> usize {
    self.hdr.msg_controllen
  }

  pub fn control(&self) -> Option<&[u8]> {
    let buf = self.control.as_ref()?;
    let len = self.control_len().min(buf.len());
    Some(&buf[..len])
  }
}
