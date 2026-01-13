//! Shared memory helpers for FastRender's multiprocess architecture.
//!
//! The implementation lives in the lightweight `fastrender-shmem` crate so it can be tested and
//! evolved without pulling in the full renderer dependency graph.
//!
//! See [`fastrender_shmem`] for backend details (POSIX `shm_open` vs Linux `memfd_create`) and
//! sandboxing rationale.

pub use fastrender_shmem::{ShmemBackend, ShmemHandle, ShmemRegion};

