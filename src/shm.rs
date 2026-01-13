//! Shared-memory buffers used by multiprocess FastRender.
//!
//! On Linux we use `memfd_create` so large buffers (frame pixels, response bodies) can be passed
//! across processes without copying them through a control channel.
//!
//! ## Security: seal mutation is a persistent DoS
//!
//! Linux file seals are *persistent* for the lifetime of the underlying memfd. When a buffer is
//! reused (pooled) across IPC boundaries, an untrusted peer could add restrictive seals such as
//! `F_SEAL_WRITE`, permanently turning the buffer read-only and breaking reuse. Locking the seal set
//! (`F_SEAL_SEAL`) after applying the required size seals prevents this class of long-lived
//! denial-of-service.
//!
//! ## Security: scrubbing pooled buffers
//!
//! If a shared-memory buffer is reused across different renderer processes / security domains, the
//! new consumer must not observe bytes written by the previous one. Linux may recycle the *same*
//! memfd across processes, so callers must explicitly scrub (zero) buffers before handing them out
//! to a different security context.
//!
//! [`SharedMemoryPool`] supports configurable scrubbing via [`ScrubPolicy`]. Scrubbing is disabled
//! by default (`ScrubPolicy::Never`) for performance; enable it when crossing trust boundaries.

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

  #[error("failed to apply required shared memory size seals: {0}")]
  SealSize(#[source] io::Error),

  #[error("failed to add shared memory seals: {0}")]
  SealAdd(#[source] io::Error),

  #[error("failed to lock shared memory seals: {0}")]
  SealLock(#[source] io::Error),

  #[error("failed to query shared memory seals: {0}")]
  SealQuery(#[source] io::Error),

  #[error("shared memory is write-sealed (F_SEAL_WRITE) and cannot be scrubbed")]
  WriteSealed,

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

  const SIZE_SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;

  /// A Linux `memfd`-backed shared memory buffer.
  #[derive(Debug)]
  pub struct SharedMemory {
    file: File,
    len: u64,
  }

  impl SharedMemory {
    /// Create a new anonymous shared memory buffer of `len` bytes.
    ///
    /// The returned memfd is always size-sealed with `F_SEAL_SHRINK | F_SEAL_GROW`.
    pub fn new(len: u64) -> Result<Self, SharedMemoryError> {
      // Use a constant C string so we don't allocate.
      let name = CStr::from_bytes_with_nul(b"fastrender-shm\0").expect("nul-terminated"); // fastrender-allow-unwrap
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

      let shm = Self { file, len };
      shm
        .add_seals_raw(SIZE_SEALS)
        .map_err(SharedMemoryError::SealSize)?;
      Ok(shm)
    }

    /// Create a new shared memory buffer and optionally lock its seal set.
    ///
    /// This always applies the required size seals (`F_SEAL_SHRINK | F_SEAL_GROW`). Callers can add
    /// additional `F_SEAL_*` bits via `seals_to_add` (e.g. `F_SEAL_WRITE`) and then optionally lock
    /// further seal mutations with `F_SEAL_SEAL`.
    pub fn create_with_seals(
      len: u64,
      seals_to_add: libc::c_int,
      lock_seals: bool,
    ) -> Result<Self, SharedMemoryError> {
      let shm = Self::new(len)?;

      let extra = seals_to_add & !(SIZE_SEALS | libc::F_SEAL_SEAL);
      if extra != 0 {
        shm
          .add_seals_raw(extra)
          .map_err(SharedMemoryError::SealAdd)?;
      }

      if lock_seals {
        shm.lock_seals()?;
      }

      Ok(shm)
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

    /// Clear the entire buffer contents to zero.
    ///
    /// This is used to scrub pooled buffers so they can be safely reused across security domains.
    ///
    /// Returns [`SharedMemoryError::WriteSealed`] when `F_SEAL_WRITE` is present.
    pub fn zero(&self) -> Result<(), SharedMemoryError> {
      let seals = self.seals()?;
      if (seals & libc::F_SEAL_WRITE) != 0 {
        return Err(SharedMemoryError::WriteSealed);
      }

      if self.len == 0 {
        return Ok(());
      }

      // A reasonably sized chunk keeps syscall count bounded without risking large stack usage.
      const CHUNK: usize = 64 * 1024; // 64 KiB
      let zeros = [0u8; CHUNK];

      let mut offset: u64 = 0;
      while offset < self.len {
        let remaining = self.len - offset;
        let to_write = remaining.min(CHUNK as u64) as usize;
        self
          .write_from_slice(offset, &zeros[..to_write])
          .map_err(SharedMemoryError::Write)?;
        offset += to_write as u64;
      }
      Ok(())
    }

    /// Returns the underlying file descriptor.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
      self.file.as_raw_fd()
    }

    /// Query the current `F_SEAL_*` bitmask.
    pub fn seals(&self) -> Result<libc::c_int, SharedMemoryError> {
      let rc = loop {
        // SAFETY: `fcntl(F_GET_SEALS)` takes no extra args.
        let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GET_SEALS) };
        if rc >= 0 {
          break rc;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(SharedMemoryError::SealQuery(err));
      };
      Ok(rc)
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
    ///
    /// This function also locks the seal set with `F_SEAL_SEAL` so untrusted peers cannot mutate the
    /// seal set after handoff.
    pub fn seal_read_only(&self) -> Result<(), SharedMemoryError> {
      let seals = self.seals()?;
      if (seals & libc::F_SEAL_WRITE) != 0 {
        // Already read-only; ensure the seal set is locked.
        return self.lock_seals();
      }

      loop {
        let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
        if rc != -1 {
          break;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        if err.raw_os_error() == Some(libc::EBUSY) {
          return Err(SharedMemoryError::SealReadOnlyBusy(err));
        }
        return Err(SharedMemoryError::SealReadOnly(err));
      }

      self.lock_seals()
    }

    /// Lock the seal set with `F_SEAL_SEAL`.
    ///
    /// This is used for pooled/shared buffers so untrusted peers cannot persistently mutate seals.
    pub fn lock_seals(&self) -> Result<(), SharedMemoryError> {
      let seals = self.seals()?;
      if (seals & libc::F_SEAL_SEAL) != 0 {
        return Ok(());
      }

      // Ensure the size seals are present before locking.
      if (seals & SIZE_SEALS) != SIZE_SEALS {
        self
          .add_seals_raw(SIZE_SEALS)
          .map_err(SharedMemoryError::SealSize)?;
      }

      self
        .add_seals_raw(libc::F_SEAL_SEAL)
        .map_err(SharedMemoryError::SealLock)
    }

    fn add_seals_raw(&self, seals: libc::c_int) -> io::Result<()> {
      loop {
        let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_ADD_SEALS, seals) };
        if rc != -1 {
          return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      }
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

    pub fn create_with_seals(
      _len: u64,
      _seals_to_add: libc::c_int,
      _lock_seals: bool,
    ) -> Result<Self, SharedMemoryError> {
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

    pub fn zero(&self) -> Result<(), SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }

    pub fn seals(&self) -> Result<libc::c_int, SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }

    pub fn seal_read_only(&self) -> Result<(), SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }

    pub fn lock_seals(&self) -> Result<(), SharedMemoryError> {
      Err(SharedMemoryError::Unsupported)
    }
  }
}

pub use imp::SharedMemory;

/// Controls when a [`SharedMemoryPool`] scrubs (zeros) buffers.
///
/// Scrubbing is required when reusing SHM buffers across different security contexts (e.g.
/// different renderer processes / origins). Without it, a new consumer can observe bytes written by
/// the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubPolicy {
  /// Never scrub buffers (fast, but unsafe across security domains).
  Never,
  /// Scrub buffers as they are returned to the pool.
  OnRelease,
  /// Scrub buffers immediately before handing them out.
  OnAcquire,
}

impl Default for ScrubPolicy {
  fn default() -> Self {
    ScrubPolicy::Never
  }
}

/// A simple pool for reusable shared memory buffers.
#[derive(Debug, Default)]
pub struct SharedMemoryPool {
  scrub_policy: ScrubPolicy,
  buffers: Vec<SharedMemory>,
}

impl SharedMemoryPool {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_scrub_policy(scrub_policy: ScrubPolicy) -> Self {
    Self {
      scrub_policy,
      buffers: Vec::new(),
    }
  }

  pub fn scrub_policy(&self) -> ScrubPolicy {
    self.scrub_policy
  }

  pub fn len(&self) -> usize {
    self.buffers.len()
  }

  /// Acquire a shared-memory buffer of `len` bytes, creating a new one if needed.
  ///
  /// When the pool is configured with [`ScrubPolicy::OnAcquire`], this method scrubs (zeros) the
  /// buffer before returning it.
  pub fn acquire(&mut self, len: u64) -> Result<SharedMemory, SharedMemoryError> {
    while let Some(buffer) = self.buffers.pop() {
      if buffer.len() != len {
        continue;
      }

      if self.scrub_policy == ScrubPolicy::OnAcquire {
        buffer.zero()?;
      }

      return Ok(buffer);
    }

    let buffer = SharedMemory::create_with_seals(len, 0, true)?;
    if self.scrub_policy == ScrubPolicy::OnAcquire {
      buffer.zero()?;
    }
    Ok(buffer)
  }

  /// Release a shared-memory buffer back to the pool for reuse.
  ///
  /// Buffers are pooled only when:
  /// - the seal set is locked (`F_SEAL_SEAL`), and
  /// - the buffer is not write-sealed (`F_SEAL_WRITE`).
  ///
  /// This prevents untrusted peers from permanently breaking reuse by mutating seals.
  ///
  /// When the pool is configured with [`ScrubPolicy::OnRelease`], this method scrubs (zeros) the
  /// buffer before pooling it.
  pub fn release(&mut self, buffer: SharedMemory) -> Result<(), SharedMemoryError> {
    if self.scrub_policy == ScrubPolicy::OnRelease {
      buffer.zero()?;
    }

    #[cfg(target_os = "linux")]
    {
      let seals = buffer.seals()?;
      if (seals & libc::F_SEAL_WRITE) != 0 {
        // Never pool write-sealed buffers; they cannot be safely reused.
        return Ok(());
      }
      if (seals & libc::F_SEAL_SEAL) == 0 {
        // Never pool buffers with an unlocked seal set; an untrusted peer could mutate seals and
        // persistently break reuse.
        return Ok(());
      }

      self.buffers.push(buffer);
      return Ok(());
    }

    #[cfg(not(target_os = "linux"))]
    {
      let _ = buffer;
      Ok(())
    }
  }

  pub fn take(&mut self) -> Option<SharedMemory> {
    while let Some(buffer) = self.buffers.pop() {
      if self.scrub_policy == ScrubPolicy::OnAcquire {
        #[cfg(target_os = "linux")]
        {
          if buffer.zero().is_err() {
            continue;
          }
        }
      }
      return Some(buffer);
    }
    None
  }

  /// Return a buffer to the pool if it is still reusable.
  ///
  /// Buffers are accepted only when the seal set is locked (`F_SEAL_SEAL`) and the buffer is *not*
  /// write-sealed (`F_SEAL_WRITE`). This prevents untrusted peers from permanently breaking reuse
  /// by mutating seals.
  pub fn put(&mut self, buffer: SharedMemory) {
    let _ = self.release(buffer);
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::{ScrubPolicy, SharedMemory, SharedMemoryError, SharedMemoryPool};
  use std::io;
  use std::os::unix::io::AsRawFd;

  #[test]
  fn shm_seal_policy_locked_buffer_prevents_adding_write_seal() {
    let shm = SharedMemory::create_with_seals(4096, 0, true).unwrap();

    let rc = unsafe { libc::fcntl(shm.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
    assert_eq!(rc, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
  }

  #[test]
  fn shm_seal_policy_pool_drops_unlocked_or_write_sealed_buffers() {
    let mut pool = SharedMemoryPool::new();

    let ok = SharedMemory::create_with_seals(4096, 0, true).unwrap();
    pool.put(ok);
    assert_eq!(pool.len(), 1);
    let _ = pool.take().expect("pooled buffer");

    let unlocked = SharedMemory::new(4096).unwrap();
    pool.put(unlocked);
    assert_eq!(pool.len(), 0);

    let write_sealed = SharedMemory::new(4096).unwrap();
    write_sealed.seal_read_only().unwrap();
    pool.put(write_sealed);
    assert_eq!(pool.len(), 0);
  }

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
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));

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
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
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
      assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
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

  #[test]
  fn shm_scrub_on_release_zeroes_reused_buffer() {
    let mut pool = SharedMemoryPool::with_scrub_policy(ScrubPolicy::OnRelease);

    // Acquire a locked, writable buffer so it is eligible for reuse.
    let shm = pool.acquire(4096).unwrap();
    shm.write_from_slice(0, &[0xA5; 4096]).unwrap();
    pool.release(shm).unwrap();

    let shm2 = pool.acquire(4096).unwrap();

    let mut readback = vec![0xFFu8; 4096];
    let rc = unsafe {
      libc::pread(
        shm2.as_raw_fd(),
        readback.as_mut_ptr().cast::<libc::c_void>(),
        readback.len(),
        0,
      )
    };
    assert_eq!(
      rc as usize,
      readback.len(),
      "expected to read entire buffer"
    );
    assert!(
      readback.iter().all(|b| *b == 0),
      "expected all bytes to be zero after scrubbing; first non-zero at {:?}",
      readback.iter().position(|b| *b != 0)
    );
  }

  #[test]
  fn shm_scrub_zero_fails_on_write_sealed_buffer() {
    let shm = SharedMemory::new(64).unwrap();
    shm.write_from_slice(0, &[0x11; 64]).unwrap();
    shm.seal_read_only().unwrap();

    let err = shm.zero().expect_err("zero should fail when write-sealed");
    assert!(
      matches!(err, SharedMemoryError::WriteSealed),
      "expected WriteSealed error, got {err:?}"
    );
  }
}
