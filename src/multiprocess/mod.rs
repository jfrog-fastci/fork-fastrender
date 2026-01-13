//! Multi-process browser architecture primitives.
//!
//! This module is an early scaffold for site isolation support (process-per-origin reuse).
//! The core type exposed today is [`registry::RendererProcessRegistry`].

pub mod registry;

pub use registry::{
  FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
  RendererProcessRegistryConfig, SiteKey,
};

#[cfg(any(test, feature = "browser_ui"))]
pub use registry::{renderer_process_count_for_test, renderer_process_spawn_count_for_test};
