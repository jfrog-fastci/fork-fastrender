//! Shared memory helpers for FastRender's IPC layer.
//!
//! The production shared-memory implementation lives in the standalone `fastrender-shmem` crate so
//! it can be reused by the renderer/broker processes without pulling in the full FastRender
//! dependency graph.
//!
//! This module is just a convenience wrapper so internal call sites can use `crate::ipc::shmem::*`
//! paths.

use std::io;

pub use fastrender_shmem::{
  generate_shmem_id, ShmemBackend, ShmemHandle, MAX_SHMEM_ID_LEN, MAX_SHMEM_NAME_LEN,
};

/// A mapped shared-memory region suitable for sharing large frame buffers across processes.
///
/// The underlying implementation lives in the standalone `fastrender-shmem` crate, but this wrapper
/// provides a simplified creation API for FastRender's IPC call sites.
pub struct ShmemRegion {
  inner: fastrender_shmem::ShmemRegion,
}

impl ShmemRegion {
  /// Create and map a new shared-memory region of `len` bytes.
  ///
  /// Security: explicitly zero-initialize the mapping before returning. Even if the OS typically
  /// provides zeroed pages for fresh shared-memory objects, making this invariant explicit avoids
  /// leaking previous-process memory (or stale named-shm contents if a name is ever reused
  /// accidentally) to an untrusted renderer.
  pub fn create(len: usize) -> io::Result<(Self, ShmemHandle)> {
    #[cfg(unix)]
    {
      let (region, handle) = fastrender_shmem::ShmemRegion::create(ShmemBackend::default(), len)?;
      let mut out = Self { inner: region };
      out.as_bytes_mut().fill(0);
      return Ok((out, handle));
    }

    #[cfg(not(unix))]
    {
      let _ = len;
      return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "shared memory is not supported on this platform",
      ));
    }
  }

  /// Map an existing shared-memory region described by `handle`.
  pub fn map(handle: &ShmemHandle) -> io::Result<Self> {
    #[cfg(unix)]
    {
      let region = fastrender_shmem::ShmemRegion::map(handle)?;
      return Ok(Self { inner: region });
    }

    #[cfg(not(unix))]
    {
      let _ = handle;
      return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "shared memory is not supported on this platform",
      ));
    }
  }

  pub fn len(&self) -> usize {
    self.inner.len()
  }

  pub fn as_bytes(&self) -> &[u8] {
    self.inner.as_slice()
  }

  pub fn as_bytes_mut(&mut self) -> &mut [u8] {
    self.inner.as_mut_slice()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generate_shmem_id_respects_macos_name_limits_and_charset() {
    for _ in 0..128 {
      let id = generate_shmem_id();
      assert!(!id.is_empty(), "id should not be empty");
      assert!(
        id.len() <= MAX_SHMEM_ID_LEN,
        "id length {} exceeded MAX_SHMEM_ID_LEN={MAX_SHMEM_ID_LEN}",
        id.len()
      );
      assert!(id.is_ascii(), "id must be ASCII: {id:?}");
      assert!(!id.contains('/'), "id must not contain '/': {id:?}");
      assert!(
        id.chars()
          .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "id contains unexpected characters: {id:?}"
      );

      // Ensure adding the leading `/` at the syscall layer still fits within macOS limits.
      let name = format!("/{id}");
      assert!(
        name.len() <= MAX_SHMEM_NAME_LEN,
        "shm_open name too long: {} bytes (max {MAX_SHMEM_NAME_LEN}): {name:?}",
        name.len()
      );
    }
  }

  #[cfg(unix)]
  #[test]
  fn shmem_region_create_zero_initializes() {
    let (region, _handle) = ShmemRegion::create(128).expect("create shared memory region");
    assert!(
      region.as_bytes().iter().all(|b| *b == 0),
      "newly created shared-memory region should be zero-initialized"
    );
  }
}
