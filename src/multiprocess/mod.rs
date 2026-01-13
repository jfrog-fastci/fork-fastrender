//! Multi-process browser architecture primitives.
//!
//! This module is an early scaffold for the multiprocess security workstream. Today it contains:
//! - [`registry`]: browser-side process-per-site/origin bookkeeping (`RendererProcessRegistry`).
//! - [`subframes`]: browser-owned frame tree + iframe discovery synchronisation, assigning frames to
//!   renderer processes by [`SiteKey`].
//! - [`compositor`]: composition of child frame pixmaps into the final tab surface.

pub mod compositor;
pub mod registry;
pub mod subframes;

pub use registry::{
  FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
  RendererProcessRegistryConfig, SiteKey,
};

pub use subframes::{
  should_isolate_child_frame, BrowserToRendererFrame, DiscoveredSubframe, FrameEmbedding, FrameNode,
  FrameTree, RendererToBrowserFrame, SubframeId, SubframesController,
};

#[cfg(any(test, feature = "browser_ui"))]
pub use registry::{renderer_process_count_for_test, renderer_process_spawn_count_for_test};
