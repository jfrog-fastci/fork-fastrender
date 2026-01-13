//! Shared-memory buffers for transferring large blobs between processes.
//!
//! The multiprocess architecture needs a way to move large payloads (frame pixels, network
//! response bodies) without copying them through a control channel. On Linux, the preferred
//! mechanism is `memfd_create` + `mmap`, with file seals applied once the producer has finished
//! writing.
//!
//! ## Design
//!
//! - [`OwnedShm`] is created by the producing process via [`OwnedShm::new`].
//! - The producer writes into [`OwnedShm::as_mut_slice`].
//! - The producer calls [`OwnedShm::seal_readonly`] as the handoff boundary.
//! - The producer passes the file descriptor over a Unix domain socket using `SCM_RIGHTS`.
//! - The consumer constructs a [`ReceivedShm`] using [`ReceivedShm::from_fd`] which validates the
//!   file size and maps it read-only.
//!
//! ## Security invariants
//!
//! - All allocations are size-checked. Sizes are capped at [`MAX_SHM_SIZE`] (256 MiB) to mitigate
//!   denial-of-service attacks.
//! - Zero-sized buffers are rejected.
//! - `ReceivedShm::from_fd` validates the received fd's size against both `expected_size` and
//!   `max_size`.
//!
//! ## Notes on seals
//!
//! Linux file seals are best-effort: on older kernels or restricted sandboxes `F_ADD_SEALS` may
//! fail. [`OwnedShm::seal_readonly`] returns a [`SealStatus`] so callers can decide whether a hard
//! sealing guarantee is required for a particular protocol.

use std::io;

/// Hard global ceiling for shared-memory buffers.
///
/// This is a defence-in-depth guardrail: callers should still pass protocol-specific maxima to
/// [`ReceivedShm::from_fd`].
pub const MAX_SHM_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealStatus {
  /// Linux seals were applied successfully (or the platform fallback enforced read-only locally).
  Applied,
  /// Seals are not available on this platform/kernel (best-effort no-op).
  Unsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum ShmError {
  #[error("shared memory size must be non-zero")]
  ZeroSize,
  #[error("shared memory size {size} exceeds maximum allowed {max}")]
  TooLarge { size: usize, max: usize },
  #[cfg(target_os = "linux")]
  #[error("memfd_create failed")]
  MemfdCreateFailed {
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("ftruncate failed for size {size}")]
  TruncateFailed {
    size: usize,
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("mmap failed for size {size}")]
  MmapFailed {
    size: usize,
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("fstat failed")]
  StatFailed {
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("shared memory size mismatch: expected {expected} bytes, got {actual}")]
  SizeMismatch { expected: usize, actual: usize },
  #[cfg(target_os = "linux")]
  #[error("shared memory fd size {actual} exceeds maximum allowed {max}")]
  SizeExceedsMax { actual: usize, max: usize },
  #[cfg(target_os = "linux")]
  #[error("failed to apply Linux seals")]
  SealFailed {
    #[source]
    source: io::Error,
  },
  #[error("shared memory is not supported on this platform")]
  Unsupported,
  #[error("shared memory buffer is sealed read-only")]
  Sealed,
}

fn validate_size(size: usize) -> Result<(), ShmError> {
  if size == 0 {
    return Err(ShmError::ZeroSize);
  }
  if size > MAX_SHM_SIZE {
    return Err(ShmError::TooLarge {
      size,
      max: MAX_SHM_SIZE,
    });
  }
  Ok(())
}

// ============================================================================
// Linux implementation (memfd + mmap + seals)
// ============================================================================

#[cfg(target_os = "linux")]
mod linux {
  use super::{validate_size, SealStatus, ShmError, MAX_SHM_SIZE};
  use crate::ipc::sync;
  use std::io;
  use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
  use std::ptr::NonNull;

  /// A mapped memory region created from a file descriptor.
  ///
  /// Invariants:
  /// - `ptr` points to a valid `len`-byte mapping returned by `mmap`.
  /// - `len > 0`.
  struct MappedRegion {
    ptr: NonNull<u8>,
    len: usize,
  }

  impl MappedRegion {
    fn map(fd: RawFd, len: usize, prot: i32) -> Result<Self, ShmError> {
      if len == 0 {
        return Err(ShmError::ZeroSize);
      }
      let addr = unsafe {
        libc::mmap(
          std::ptr::null_mut(),
          len,
          prot,
          libc::MAP_SHARED,
          fd,
          0,
        )
      };
      if addr == libc::MAP_FAILED {
        return Err(ShmError::MmapFailed {
          size: len,
          source: io::Error::last_os_error(),
        });
      }

      let Some(ptr) = NonNull::new(addr.cast::<u8>()) else {
        // Extremely unlikely: mapping succeeded at address 0. Treat as an error to preserve the
        // non-null invariant required by `slice::from_raw_parts`.
        unsafe {
          let _ = libc::munmap(addr, len);
        }
        return Err(ShmError::MmapFailed {
          size: len,
          source: io::Error::new(io::ErrorKind::Other, "mmap returned a null pointer"),
        });
      };
      Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
      // SAFETY: `ptr` is a valid mapping for `len` bytes for the lifetime of `self`.
      unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
      // SAFETY: `ptr` is a valid mapping for `len` bytes for the lifetime of `self`.
      unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    fn mprotect_readonly(&self) -> io::Result<()> {
      // SAFETY: `ptr`/`len` describe a valid mapping.
      let rc =
        unsafe { libc::mprotect(self.ptr.as_ptr().cast::<libc::c_void>(), self.len, libc::PROT_READ) };
      if rc != 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    }
  }

  impl Drop for MappedRegion {
    fn drop(&mut self) {
      // SAFETY: `ptr`/`len` describe a valid mapping. Drop must not panic; ignore failures.
      unsafe {
        let _ = libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.len);
      }
    }
  }

  /// Producer-side shared-memory buffer backed by Linux `memfd`.
  ///
  /// The mapping starts read-write; after calling [`OwnedShm::seal_readonly`] the object becomes
  /// logically read-only (and attempts to obtain a mutable slice will fail).
  pub struct OwnedShm {
    fd: OwnedFd,
    region: MappedRegion,
    sealed: bool,
  }

  impl OwnedShm {
    pub fn new(size: usize) -> Result<Self, ShmError> {
      validate_size(size)?;

      let off: libc::off_t = size
        .try_into()
        .map_err(|_| ShmError::TooLarge { size, max: MAX_SHM_SIZE })?;

      // We use a fixed name; it is only visible in `/proc/<pid>/fd` and for debugging.
      let name = b"fastrender-shm\0";

      let mut fd = unsafe {
        libc::memfd_create(
          name.as_ptr().cast::<libc::c_char>(),
          libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
      };
      if fd < 0 {
        let err = io::Error::last_os_error();
        // Older kernels may reject `MFD_ALLOW_SEALING`. Fall back to an unsealable memfd so the
        // caller can still use shared memory (sealing will report `Unsupported`).
        if err.raw_os_error() == Some(libc::EINVAL) {
          fd = unsafe {
            libc::memfd_create(name.as_ptr().cast::<libc::c_char>(), libc::MFD_CLOEXEC)
          };
        }
        if fd < 0 {
          return Err(ShmError::MemfdCreateFailed {
            source: io::Error::last_os_error(),
          });
        }
      }

      // SAFETY: `fd` is freshly returned by the kernel; we own it.
      let fd = unsafe { OwnedFd::from_raw_fd(fd) };

      // SAFETY: `ftruncate` uses the provided fd and length.
      let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), off) };
      if rc != 0 {
        return Err(ShmError::TruncateFailed {
          size,
          source: io::Error::last_os_error(),
        });
      }

      let mut region = MappedRegion::map(fd.as_raw_fd(), size, libc::PROT_READ | libc::PROT_WRITE)?;
      // Security: make the invariant explicit that freshly-created shared-memory mappings start
      // zeroed. While kernels typically provide zeroed pages for new allocations, explicitly
      // clearing the mapping avoids leaking previous-process memory (or stale shared-memory
      // contents) to an untrusted renderer if a name/fd is ever reused accidentally.
      region.as_mut_slice().fill(0);
      Ok(Self {
        fd,
        region,
        sealed: false,
      })
    }

    pub fn size(&self) -> usize {
      self.region.len
    }

    /// Borrow the underlying fd for passing via `SCM_RIGHTS`.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
      self.fd.as_fd()
    }

    pub fn as_slice(&self) -> &[u8] {
      self.region.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], ShmError> {
      if self.sealed {
        return Err(ShmError::Sealed);
      }
      Ok(self.region.as_mut_slice())
    }

    /// Attempt to seal the backing `memfd` read-only.
    ///
    /// On kernels that support seals, this applies:
    /// - `F_SEAL_SHRINK` / `F_SEAL_GROW` (size is immutable)
    /// - `F_SEAL_WRITE` (contents are immutable)
    /// - `F_SEAL_SEAL` (seal set is immutable; best-effort defense-in-depth)
    ///
    /// Even when seals are unsupported, this method still transitions the object into a
    /// read-only state for *this* process (future calls to [`OwnedShm::as_mut_slice`] will return
    /// [`ShmError::Sealed`]).
    pub fn seal_readonly(&mut self) -> Result<SealStatus, ShmError> {
      if self.sealed {
        return Ok(SealStatus::Applied);
      }

      self.sealed = true;

      // Publish all shared-memory writes performed by the producer (pixel buffer, response body,
      // etc.) before we hand the memfd off to another process. The actual readiness signal travels
      // over a separate IPC channel; this fence provides the necessary ordering point.
      sync::shm_publish_frame();

      // Best-effort: make our mapping read-only to avoid accidental writes in the producer.
      let _ = self.region.mprotect_readonly();

      // Apply the required immutability seals first so they still take effect even if locking the
      // seal set (`F_SEAL_SEAL`) is unsupported/blocked by sandbox policy.
      let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
      let rc = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_ADD_SEALS, required) };
      if rc != 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
          // Kernel doesn't support sealing or the file isn't sealable (older kernels / seccomp).
          Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => Ok(SealStatus::Unsupported),
          _ => Err(ShmError::SealFailed { source: err }),
        };
      }

      // Best-effort defense-in-depth: lock the seal set.
      let rc = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SEAL) };
      if rc != 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
          Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => {}
          _ => return Err(ShmError::SealFailed { source: err }),
        }
      }

      Ok(SealStatus::Applied)
    }
  }

  impl AsFd for OwnedShm {
    fn as_fd(&self) -> BorrowedFd<'_> {
      self.fd.as_fd()
    }
  }

  /// Consumer-side read-only shared-memory mapping.
  pub struct ReceivedShm {
    _fd: OwnedFd,
    region: MappedRegion,
  }

  impl ReceivedShm {
    pub fn from_fd(
      fd: OwnedFd,
      expected_size: usize,
      max_size: usize,
    ) -> Result<Self, ShmError> {
      validate_size(expected_size)?;

      let effective_max = std::cmp::min(max_size, MAX_SHM_SIZE);
      if expected_size > effective_max {
        return Err(ShmError::TooLarge {
          size: expected_size,
          max: effective_max,
        });
      }

      let mut st: libc::stat = unsafe { std::mem::zeroed() };
      // SAFETY: `fstat` writes to `st` when pointer is valid.
      let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
      if rc != 0 {
        return Err(ShmError::StatFailed {
          source: io::Error::last_os_error(),
        });
      }

      let raw_size = st.st_size;
      if raw_size < 0 {
        return Err(ShmError::SizeMismatch {
          expected: expected_size,
          actual: 0,
        });
      }
      let actual_size: usize = (raw_size as u64)
        .try_into()
        .map_err(|_| ShmError::SizeExceedsMax {
          actual: usize::MAX,
          max: effective_max,
        })?;

      if actual_size > effective_max {
        return Err(ShmError::SizeExceedsMax {
          actual: actual_size,
          max: effective_max,
        });
      }

      if actual_size != expected_size {
        return Err(ShmError::SizeMismatch {
          expected: expected_size,
          actual: actual_size,
        });
      }

      let region = MappedRegion::map(fd.as_raw_fd(), actual_size, libc::PROT_READ)?;
      Ok(Self { _fd: fd, region })
    }

    pub fn size(&self) -> usize {
      self.region.len
    }

    pub fn as_slice(&self) -> &[u8] {
      // Ensure we don't speculatively read from the mapping until after we've observed the IPC
      // signal that transferred/announced this memfd. Pair with the producer-side Release fence in
      // `OwnedShm::seal_readonly`.
      sync::shm_consume_frame();
      self.region.as_slice()
    }
  }
}

// ============================================================================
// Non-Linux fallback (in-process Vec<u8>)
// ============================================================================

#[cfg(not(target_os = "linux"))]
mod portable {
  use super::{validate_size, SealStatus, ShmError};
  use crate::ipc::sync;

  /// Portable fallback that stores bytes inline.
  ///
  /// This is *not* backed by OS shared memory and therefore cannot be passed across processes via
  /// fd passing. It exists so the crate compiles on non-Linux targets while the multiprocess
  /// backend is Linux-first.
  pub struct OwnedShm {
    buf: Vec<u8>,
    sealed: bool,
  }

  impl OwnedShm {
    pub fn new(size: usize) -> Result<Self, ShmError> {
      validate_size(size)?;
      Ok(Self {
        buf: vec![0u8; size],
        sealed: false,
      })
    }

    pub fn size(&self) -> usize {
      self.buf.len()
    }

    pub fn as_slice(&self) -> &[u8] {
      &self.buf
    }

    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], ShmError> {
      if self.sealed {
        return Err(ShmError::Sealed);
      }
      Ok(&mut self.buf)
    }

    pub fn seal_readonly(&mut self) -> Result<SealStatus, ShmError> {
      sync::shm_publish_frame();
      self.sealed = true;
      Ok(SealStatus::Unsupported)
    }
  }

  pub struct ReceivedShm {
    buf: Vec<u8>,
  }

  impl ReceivedShm {
    #[cfg(unix)]
    pub fn from_fd(
      _fd: std::os::fd::OwnedFd,
      _expected_size: usize,
      _max_size: usize,
    ) -> Result<Self, ShmError> {
      Err(ShmError::Unsupported)
    }

    pub fn size(&self) -> usize {
      self.buf.len()
    }

    pub fn as_slice(&self) -> &[u8] {
      sync::shm_consume_frame();
      &self.buf
    }
  }
}

#[cfg(target_os = "linux")]
pub use linux::{OwnedShm, ReceivedShm};
#[cfg(not(target_os = "linux"))]
pub use portable::{OwnedShm, ReceivedShm};

// ============================================================================
// Tests (Linux-only)
// ============================================================================

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use crate::ipc::ancillary::{recv_fd, send_fd};
  use crate::ipc::sync;
  use std::os::unix::net::UnixStream;

  #[test]
  fn memfd_roundtrip_over_scm_rights() {
    let publish_before = sync::shm_publish_count_for_test();
    let consume_before = sync::shm_consume_count_for_test();

    let mut shm = OwnedShm::new(4096).expect("create shm");
    let buf = shm.as_mut_slice().expect("mutable slice");
    for (i, b) in buf.iter_mut().enumerate() {
      *b = (i % 251) as u8;
    }

    let _ = shm.seal_readonly().expect("seal");
    assert!(
      sync::shm_publish_count_for_test() > publish_before,
      "expected publish fence to run when sealing shared memory"
    );

    let (a, b) = UnixStream::pair().expect("socketpair");
    send_fd(&a, shm.as_fd()).expect("send fd");
    let received_fd = recv_fd(&b).expect("recv fd");

    let received =
      ReceivedShm::from_fd(received_fd, 4096, MAX_SHM_SIZE).expect("map received shm");
    assert_eq!(received.size(), 4096);
    let received_bytes = received.as_slice();
    assert!(
      sync::shm_consume_count_for_test() > consume_before,
      "expected consume fence to run when reading shared memory"
    );
    assert_eq!(received_bytes, shm.as_slice());
  }

  #[test]
  fn size_mismatch_is_rejected() {
    let shm = OwnedShm::new(1024).expect("create shm");
    let (a, b) = UnixStream::pair().expect("socketpair");
    send_fd(&a, shm.as_fd()).expect("send fd");
    let received_fd = recv_fd(&b).expect("recv fd");

    match ReceivedShm::from_fd(received_fd, 2048, MAX_SHM_SIZE) {
      Err(ShmError::SizeMismatch { expected, actual }) => {
        assert_eq!(expected, 2048);
        assert_eq!(actual, 1024);
      }
      Err(other) => panic!("unexpected error variant: {other:?}"),
      Ok(_) => panic!("expected size mismatch error"),
    };
  }

  #[test]
  fn owned_shm_new_zero_initializes() {
    let shm = OwnedShm::new(256).expect("create shm");
    assert!(
      shm.as_slice().iter().all(|b| *b == 0),
      "newly created shared-memory mappings should be zero-initialized"
    );
  }
}
