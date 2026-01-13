//! Linux shared memory backed by `memfd_create` + `mmap`.
//!
//! This wrapper is intended for allocating fixed-size buffers that can be passed between
//! processes (e.g. via `SCM_RIGHTS`). The creator seals the memfd against resizing so the
//! receiver cannot shrink/grow the underlying file.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

/// Shared memory allocation backed by a sealed memfd.
#[derive(Debug)]
pub struct SharedMemory {
  fd: OwnedFd,
  size: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SharedMemoryError {
  #[error("shared memory size is zero")]
  SizeIsZero,
  #[error("shared memory size {size} exceeds maximum {max}")]
  SizeTooLarge { size: u64, max: u64 },
  #[error("shared memory size {size} does not fit in platform offset type")]
  SizeDoesNotFitInOffset { size: u64 },
  #[error("failed to create memfd")]
  MemfdCreateFailed {
    #[source]
    source: io::Error,
  },
  #[error("failed to truncate memfd to {size} bytes")]
  FtruncateFailed {
    size: u64,
    #[source]
    source: io::Error,
  },
  #[error("failed to apply size seals to memfd")]
  SealSizeFailed {
    #[source]
    source: io::Error,
  },
  #[error("failed to stat shared memory fd")]
  FstatFailed {
    #[source]
    source: io::Error,
  },
  #[error("shared memory fd is not a regular file (mode {mode:#o})")]
  NotRegularFile { mode: libc::mode_t },
  #[error("shared memory size {size} does not fit in usize")]
  SizeDoesNotFitInUsize { size: u64 },
  #[error("failed to query memfd seals")]
  GetSealsFailed {
    #[source]
    source: io::Error,
  },
  #[error("mmap failed")]
  MmapFailed {
    #[source]
    source: io::Error,
  },
  #[error("mmap returned a null pointer")]
  MmapReturnedNull,
}

const MEMFD_NAME: &[u8] = b"fastrender-shm\0";

// Keep the SHM allocation limit aligned with the existing pixmap allocation guardrail.
const MAX_SHM_BYTES: u64 = crate::paint::pixmap::MAX_PIXMAP_BYTES;

impl SharedMemory {
  /// Create a new shared memory allocation of `size` bytes.
  pub fn create(size: usize) -> Result<Self, SharedMemoryError> {
    let size_u64 = validate_size(size)?;

    // SAFETY: `MEMFD_NAME` is nul-terminated and the flags are Linux-defined.
    let raw_fd = unsafe {
      libc::memfd_create(
        MEMFD_NAME.as_ptr().cast::<libc::c_char>(),
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
      )
    };
    if raw_fd < 0 {
      return Err(SharedMemoryError::MemfdCreateFailed {
        source: io::Error::last_os_error(),
      });
    }

    // SAFETY: `raw_fd` is owned by us and is valid when non-negative.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let size_off: libc::off_t = size_u64
      .try_into()
      .map_err(|_| SharedMemoryError::SizeDoesNotFitInOffset { size: size_u64 })?;

    // SAFETY: `ftruncate` operates on the fd and we pass a size that fits in `off_t`.
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), size_off) };
    if rc != 0 {
      return Err(SharedMemoryError::FtruncateFailed {
        size: size_u64,
        source: io::Error::last_os_error(),
      });
    }

    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    // SAFETY: `fcntl` is called with a valid fd and a correct seal bitmask argument.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) };
    if rc != 0 {
      return Err(SharedMemoryError::SealSizeFailed {
        source: io::Error::last_os_error(),
      });
    }

    // Security: make the invariant explicit that newly created shared-memory regions start
    // zero-initialized. Even if the kernel typically provides zeroed pages, explicitly clearing the
    // mapping avoids leaking stale bytes (e.g. previous-process memory) if an fd is ever reused
    // accidentally.
    let mut view = map_fd(
      fd.as_raw_fd(),
      size,
      libc::PROT_READ | libc::PROT_WRITE,
    )
    .map(MmapViewMut)?;
    view.as_mut_slice().fill(0);

    Ok(Self { fd, size })
  }

  /// Wrap an existing fd that refers to a regular file (e.g. a received memfd).
  ///
  /// This validates the file type and size (including upper bound) so mapping cannot
  /// accidentally request huge or overflowing lengths.
  pub fn from_fd(fd: OwnedFd) -> Result<Self, SharedMemoryError> {
    let (size, mode) = stat_fd(fd.as_raw_fd())?;
    if (mode & libc::S_IFMT) != libc::S_IFREG {
      return Err(SharedMemoryError::NotRegularFile { mode });
    }
    let size_u64 = validate_existing_size(size)?;
    let size_usize: usize = size_u64
      .try_into()
      .map_err(|_| SharedMemoryError::SizeDoesNotFitInUsize { size: size_u64 })?;
    Ok(Self { fd, size: size_usize })
  }

  pub fn size(&self) -> usize {
    self.size
  }

  pub fn as_fd(&self) -> BorrowedFd<'_> {
    self.fd.as_fd()
  }

  /// Consumes the wrapper and returns the owned file descriptor.
  pub fn into_fd(self) -> OwnedFd {
    let Self { fd, .. } = self;
    fd
  }

  /// Returns the memfd seal bitmask (`F_GET_SEALS`).
  pub fn seals(&self) -> Result<i32, SharedMemoryError> {
    // SAFETY: `fcntl` is called with a valid fd and the correct command.
    let rc = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GET_SEALS) };
    if rc < 0 {
      return Err(SharedMemoryError::GetSealsFailed {
        source: io::Error::last_os_error(),
      });
    }
    Ok(rc)
  }

  pub fn map_read_only(&self) -> Result<MmapView, SharedMemoryError> {
    map_fd(self.fd.as_raw_fd(), self.size, libc::PROT_READ).map(MmapView)
  }

  pub fn map_read_write(&self) -> Result<MmapViewMut, SharedMemoryError> {
    map_fd(
      self.fd.as_raw_fd(),
      self.size,
      libc::PROT_READ | libc::PROT_WRITE,
    )
    .map(MmapViewMut)
  }
}

impl AsFd for SharedMemory {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.fd.as_fd()
  }
}

impl AsRawFd for SharedMemory {
  fn as_raw_fd(&self) -> std::os::fd::RawFd {
    self.fd.as_raw_fd()
  }
}

fn validate_size(size: usize) -> Result<u64, SharedMemoryError> {
  if size == 0 {
    return Err(SharedMemoryError::SizeIsZero);
  }
  let size_u64: u64 = size as u64;
  if size_u64 > MAX_SHM_BYTES {
    return Err(SharedMemoryError::SizeTooLarge {
      size: size_u64,
      max: MAX_SHM_BYTES,
    });
  }
  Ok(size_u64)
}

fn validate_existing_size(size: u64) -> Result<u64, SharedMemoryError> {
  if size == 0 {
    return Err(SharedMemoryError::SizeIsZero);
  }
  if size > MAX_SHM_BYTES {
    return Err(SharedMemoryError::SizeTooLarge {
      size,
      max: MAX_SHM_BYTES,
    });
  }
  Ok(size)
}

fn stat_fd(fd: libc::c_int) -> Result<(u64, libc::mode_t), SharedMemoryError> {
  let mut stat: libc::stat = unsafe { std::mem::zeroed() };
  // SAFETY: `fstat` writes to the provided `stat` when it is a valid pointer.
  let rc = unsafe { libc::fstat(fd, &mut stat) };
  if rc != 0 {
    return Err(SharedMemoryError::FstatFailed {
      source: io::Error::last_os_error(),
    });
  }

  let size_u64 = u64::try_from(stat.st_size).map_err(|_| SharedMemoryError::FstatFailed {
    source: io::Error::new(io::ErrorKind::InvalidData, "negative st_size for shared memory fd"),
  })?;

  Ok((size_u64, stat.st_mode))
}

#[derive(Debug)]
struct MmapInner {
  ptr: NonNull<u8>,
  len: usize,
}

fn map_fd(fd: libc::c_int, len: usize, prot: libc::c_int) -> Result<MmapInner, SharedMemoryError> {
  if len == 0 {
    return Err(SharedMemoryError::SizeIsZero);
  }

  // SAFETY:
  // - `mmap` is called with a valid fd and a page-aligned offset (0).
  // - The returned mapping is validated against `MAP_FAILED`.
  let ptr = unsafe {
    libc::mmap(
      std::ptr::null_mut(),
      len,
      prot,
      libc::MAP_SHARED,
      fd,
      0,
    )
  };

  if ptr == libc::MAP_FAILED {
    return Err(SharedMemoryError::MmapFailed {
      source: io::Error::last_os_error(),
    });
  }

  let ptr = NonNull::new(ptr.cast::<u8>()).ok_or(SharedMemoryError::MmapReturnedNull)?;
  Ok(MmapInner { ptr, len })
}

/// Read-only memory-mapped view of a [`SharedMemory`] buffer.
#[derive(Debug)]
pub struct MmapView(MmapInner);

impl MmapView {
  pub fn as_slice(&self) -> &[u8] {
    // SAFETY: the mapping is valid for `len` bytes until we call `munmap` in `Drop`.
    unsafe { std::slice::from_raw_parts(self.0.ptr.as_ptr(), self.0.len) }
  }

  pub fn len(&self) -> usize {
    self.0.len
  }

  pub fn is_empty(&self) -> bool {
    self.0.len == 0
  }
}

impl std::ops::Deref for MmapView {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    self.as_slice()
  }
}

impl Drop for MmapView {
  fn drop(&mut self) {
    // SAFETY: `ptr` is a valid mapping pointer and `len` matches the original mmap length.
    let _ = unsafe { libc::munmap(self.0.ptr.as_ptr().cast::<libc::c_void>(), self.0.len) };
  }
}

/// Read-write memory-mapped view of a [`SharedMemory`] buffer.
#[derive(Debug)]
pub struct MmapViewMut(MmapInner);

impl MmapViewMut {
  pub fn as_slice(&self) -> &[u8] {
    // SAFETY: the mapping is valid for `len` bytes until we call `munmap` in `Drop`.
    unsafe { std::slice::from_raw_parts(self.0.ptr.as_ptr(), self.0.len) }
  }

  pub fn as_mut_slice(&mut self) -> &mut [u8] {
    // SAFETY: the mapping is valid for `len` bytes until we call `munmap` in `Drop`.
    unsafe { std::slice::from_raw_parts_mut(self.0.ptr.as_ptr(), self.0.len) }
  }

  pub fn len(&self) -> usize {
    self.0.len
  }

  pub fn is_empty(&self) -> bool {
    self.0.len == 0
  }
}

impl std::ops::Deref for MmapViewMut {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    self.as_slice()
  }
}

impl std::ops::DerefMut for MmapViewMut {
  fn deref_mut(&mut self) -> &mut Self::Target {
    self.as_mut_slice()
  }
}

impl Drop for MmapViewMut {
  fn drop(&mut self) {
    // SAFETY: `ptr` is a valid mapping pointer and `len` matches the original mmap length.
    let _ = unsafe { libc::munmap(self.0.ptr.as_ptr().cast::<libc::c_void>(), self.0.len) };
  }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;

  #[test]
  fn shared_memory_create_map_rw_ro() -> Result<(), SharedMemoryError> {
    let shm = SharedMemory::create(4096)?;

    {
      let mut view = shm.map_read_write()?;
      view.as_mut_slice()[0..4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    }

    let view_ro = shm.map_read_only()?;
    assert_eq!(&view_ro.as_slice()[0..4], &[0x11, 0x22, 0x33, 0x44]);
    Ok(())
  }

  #[test]
  fn shared_memory_size_seals_prevent_ftruncate() -> Result<(), SharedMemoryError> {
    let shm = SharedMemory::create(4096)?;
    let fd = shm.as_raw_fd();

    let seals = shm.seals()?;
    assert_ne!(seals & libc::F_SEAL_SHRINK, 0);
    assert_ne!(seals & libc::F_SEAL_GROW, 0);

    // SAFETY: `fd` is a live file descriptor.
    let rc = unsafe { libc::ftruncate(fd, 8192) };
    assert_eq!(rc, -1);
    let err = io::Error::last_os_error();
    assert_eq!(err.raw_os_error(), Some(libc::EPERM));

    // SAFETY: `fd` is a live file descriptor.
    let rc = unsafe { libc::ftruncate(fd, 1024) };
    assert_eq!(rc, -1);
    let err = io::Error::last_os_error();
    assert_eq!(err.raw_os_error(), Some(libc::EPERM));

    Ok(())
  }

  #[test]
  fn shared_memory_create_zero_initializes() -> Result<(), SharedMemoryError> {
    let shm = SharedMemory::create(256)?;
    let view = shm.map_read_only()?;
    assert!(
      view.as_slice().iter().all(|b| *b == 0),
      "newly created shared memory should be zero-initialized"
    );
    Ok(())
  }

  #[test]
  fn shared_memory_from_fd_rejects_non_regular() {
    let mut fds = [0; 2];
    // SAFETY: `fds` points to two valid integers that `pipe` can fill.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0);

    // SAFETY: `pipe` initialized these file descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: `pipe` initialized these file descriptors.
    let _write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    let result = SharedMemory::from_fd(read_fd);
    assert!(matches!(result, Err(SharedMemoryError::NotRegularFile { .. })));
  }
}
