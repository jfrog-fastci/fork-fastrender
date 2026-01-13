//! Shared memory helpers for FastRender's IPC layer.
//!
//! The production shared-memory implementation lives in the standalone `fastrender-shmem` crate so
//! it can be reused by the renderer/broker processes without pulling in the full FastRender
//! dependency graph.
//!
//! This module is just a convenience wrapper so internal call sites can use `crate::ipc::shmem::*`
//! paths.

pub use fastrender_shmem::{
  generate_shmem_id, ShmemBackend, ShmemHandle, ShmemRegion, MAX_SHMEM_ID_LEN, MAX_SHMEM_NAME_LEN,
};

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
    let (region, _handle) =
      ShmemRegion::create(ShmemBackend::default(), 128).expect("create shared memory region");
    assert!(
      region.as_slice().iter().all(|b| *b == 0),
      "newly created shared-memory region should be zero-initialized"
    );
  }
}
