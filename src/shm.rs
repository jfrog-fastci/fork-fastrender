//! Shared memory utilities for multiprocess IPC.
//!
//! The initial implementation targets Linux and uses `memfd` so buffers can be passed across
//! processes without hitting the filesystem. A producer can fill the buffer and then apply
//! write-seals before handing it to an untrusted consumer.

use std::io;

use thiserror::Error;

/// Errors returned by [`SharedMemory`].
#[derive(Debug, Error)]
pub enum SharedMemoryError {
  #[error("shared memory is not supported on this platform")]
  Unsupported,

  #[error("shared memory buffer is too large ({0} bytes)")]
  LengthOverflow(usize),

  #[error("failed to create memfd: {0}")]
  Create(#[source] io::Error),

  #[error("failed to resize shared memory to {size} bytes: {source}")]
  Resize {
    size: u64,
    #[source]
    source: io::Error,
  },

  #[error("failed to write shared memory contents: {0}")]
  Write(#[source] io::Error),

  #[error(
    "cannot seal shared memory as read-only while writable mappings exist (unmap writable views before sealing): {0}"
  )]
  SealReadOnlyBusy(#[source] io::Error),

  #[error("failed to seal shared memory as read-only: {0}")]
  SealReadOnly(#[source] io::Error),
}

// ============================================================================
// Linux implementation
// ============================================================================

#[cfg(target_os = "linux")]
mod imp {
  use super::SharedMemoryError;
  use std::ffi::CStr;
  use std::fs::File;
  use std::io;
  use std::os::unix::fs::FileExt;
  use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

  /// A Linux `memfd`-backed shared memory buffer.
  #[derive(Debug)]
  pub struct SharedMemory {
    file: File,
    len: u64,
  }

  impl SharedMemory {
    /// Create a new anonymous shared memory buffer of `len` bytes.
    pub fn new(len: u64) -> Result<Self, SharedMemoryError> {
      // Use a constant C string so we don't allocate.
      let name = CStr::from_bytes_with_nul(b"fastrender-shm\0").expect("nul-terminated");
      // `MFD_ALLOW_SEALING` is required to later apply `F_SEAL_*` seals.
      let fd = unsafe {
        libc::memfd_create(
          name.as_ptr(),
          libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
      };
      if fd < 0 {
        return Err(SharedMemoryError::Create(io::Error::last_os_error()));
      }

      // SAFETY: `fd` is a fresh FD owned by this function.
      let file = unsafe { File::from_raw_fd(fd) };
      file
        .set_len(len)
        .map_err(|source| SharedMemoryError::Resize { size: len, source })?;

      Ok(Self { file, len })
    }

    /// Create a sealed read-only shared memory buffer containing `data`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, SharedMemoryError> {
      let len = u64::try_from(data.len()).map_err(|_| SharedMemoryError::LengthOverflow(data.len()))?;
      let shm = Self::new(len)?;
      shm
        .write_from_slice(0, data)
        .map_err(SharedMemoryError::Write)?;
      shm.seal_read_only()?;
      Ok(shm)
    }

    /// Returns the backing buffer length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Returns the underlying file descriptor.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
      self.file.as_raw_fd()
    }

    /// Copy `data` into this shared memory buffer at `offset`.
    ///
    /// This uses `pwrite` so it does not affect the file offset.
    pub fn write_from_slice(&self, offset: u64, data: &[u8]) -> io::Result<()> {
      let data_len = u64::try_from(data.len()).map_err(|_| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("write_from_slice data length too large ({})", data.len()),
        )
      })?;
      let end = offset.checked_add(data_len).ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          "write_from_slice offset overflow",
        )
      })?;
      if end > self.len {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!(
            "write_from_slice out of bounds (offset={offset}, len={}, data_len={})",
            self.len,
            data.len()
          ),
        ));
      }

      let mut written = 0usize;
      while written < data.len() {
        let n = self
          .file
          .write_at(&data[written..], offset + written as u64)?;
        if n == 0 {
          return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write to shared memory (write_at returned 0)",
          ));
        }
        written += n;
      }

      Ok(())
    }

    /// Prevent further modifications to this shared memory buffer.
    ///
    /// On Linux this adds the `F_SEAL_WRITE` seal. The kernel rejects the seal with `EBUSY` if there
    /// are any writable memory mappings of the buffer. A typical pattern is:
    ///
    /// 1. Map the buffer writable (`PROT_WRITE`) and fill it.
    /// 2. Drop/unmap all writable mappings.
    /// 3. Call `seal_read_only()`.
    pub fn seal_read_only(&self) -> Result<(), SharedMemoryError> {
      let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
      if rc == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EBUSY) {
          return Err(SharedMemoryError::SealReadOnlyBusy(err));
        }
        return Err(SharedMemoryError::SealReadOnly(err));
      }
      Ok(())
    }
  }

  impl AsRawFd for SharedMemory {
    fn as_raw_fd(&self) -> RawFd {
      self.file.as_raw_fd()
    }
  }

}

// ============================================================================
// Stub implementation for non-Linux targets
// ============================================================================

#[cfg(not(target_os = "linux"))]
mod imp {
  use super::SharedMemoryError;
  use std::io;

  /// Shared memory is currently only implemented on Linux.
  #[derive(Debug)]
  pub struct SharedMemory;

  impl SharedMemory {
    pub fn new(_len: u64) -> Result<Self, SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }

    pub fn from_bytes(_data: &[u8]) -> Result<Self, SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }

    #[must_use]
    pub fn len(&self) -> u64 {
      0
    }

    pub fn write_from_slice(&self, _offset: u64, _data: &[u8]) -> io::Result<()> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SharedMemory is only implemented on Linux",
      ))
    }

    pub fn seal_read_only(&self) -> Result<(), SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }
  }

}

pub use imp::SharedMemory;

// ============================================================================
// Tests
// ============================================================================

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::{SharedMemory, SharedMemoryError};
  use std::io;
  use std::os::unix::io::AsRawFd;

  #[test]
  fn shm_seal_write_write_and_pwrite_fail_with_eperm() {
    let shm = SharedMemory::new(8).unwrap();
    shm.write_from_slice(0, b"abcdefg").unwrap();
    shm.seal_read_only().unwrap();

    let byte = [b'X'];

    // `write(2)` should fail once F_SEAL_WRITE is applied.
    let rc =
      unsafe { libc::write(shm.as_raw_fd(), byte.as_ptr().cast::<libc::c_void>(), byte.len()) };
    assert_eq!(rc, -1);
    assert_eq!(
      io::Error::last_os_error().raw_os_error(),
      Some(libc::EPERM)
    );

    // `pwrite(2)` should also fail.
    let rc = unsafe {
      libc::pwrite(
        shm.as_raw_fd(),
        byte.as_ptr().cast::<libc::c_void>(),
        byte.len(),
        0,
      )
    };
    assert_eq!(rc, -1);
    assert_eq!(
      io::Error::last_os_error().raw_os_error(),
      Some(libc::EPERM)
    );
  }

  #[test]
  fn shm_seal_write_mmap_prot_write_fails_with_eperm() {
    let shm = SharedMemory::new(4096).unwrap();
    shm.write_from_slice(0, b"hello").unwrap();
    shm.seal_read_only().unwrap();

    unsafe {
      let addr = libc::mmap(
        std::ptr::null_mut(),
        4096,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        shm.as_raw_fd(),
        0,
      );
      assert_eq!(addr, libc::MAP_FAILED);
      assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
      );
    }
  }

  #[test]
  fn shm_seal_write_busy_when_writable_mapping_is_alive() {
    let shm = SharedMemory::new(4096).unwrap();

    unsafe {
      let addr = libc::mmap(
        std::ptr::null_mut(),
        4096,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        shm.as_raw_fd(),
        0,
      );
      assert_ne!(addr, libc::MAP_FAILED);

      let err = shm.seal_read_only().unwrap_err();
      match err {
        SharedMemoryError::SealReadOnlyBusy(source) => {
          assert_eq!(source.raw_os_error(), Some(libc::EBUSY));
        }
        other => panic!("expected SealReadOnlyBusy, got {other:?}"),
      }

      assert_eq!(libc::munmap(addr, 4096), 0);
    }

    // Now sealing should succeed.
    shm.seal_read_only().unwrap();
  }
}
