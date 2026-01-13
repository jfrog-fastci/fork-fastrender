//! Multi-process browser architecture primitives.
//!
//! This module is a small scaffold for site isolation / process management plus
//! browser-side utilities that are required once rendering happens out-of-process.
//! Today it contains:
//! - [`registry`]: process-per-site/origin bookkeeping (`RendererProcessRegistry`).
//! - [`compositor`]: composition of child frame pixmaps into the final tab surface.

pub mod compositor;
pub mod registry;

pub use registry::{
  FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
  RendererProcessRegistryConfig, SiteKey,
};

#[cfg(any(test, feature = "browser_ui"))]
pub use registry::{renderer_process_count_for_test, renderer_process_spawn_count_for_test};

