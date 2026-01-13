//! Multi-process browser architecture primitives.
//!
//! This module is an early scaffold for the multiprocess security workstream. Today it contains:
//! - [`registry`]: browser-side process-per-site/origin bookkeeping (`RendererProcessRegistry`).
//! - [`subframes`]: browser-owned frame tree + iframe discovery synchronisation, assigning frames to
//!   renderer processes by [`SiteKey`].
//! - [`frame_tree`]: low-level browsing-context tree representation keyed by [`FrameToken`], with
//!   configurable iframe depth limiting (used by future OOPIF + site isolation plumbing).
//! - [`compositor`]: composition of child frame pixmaps into the final tab surface.
//! - [`network`]: in-process network service helpers for ResourceFetcher APIs.
//! - [`network_fetch`]: cancellable Browser↔Network fetch IPC primitives.
//! - [`shmem`]: shared-memory helpers (Linux `memfd_create` + FD inheritance, etc.).

pub mod compositor;
pub mod frame_tree;
pub mod network;
pub mod network_fetch;
pub mod registry;
pub mod subframes;
pub mod shmem;

pub use frame_tree::{EmbeddingGeometry, FrameNodeStatus, FrameToken};
pub use registry::{
  FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
  RendererProcessRegistryConfig, SiteKey,
};

pub use subframes::{
  should_isolate_child_frame, BrowserToRendererFrame, DiscoveredSubframe, FrameEmbedding, FrameNode,
  FrameTree, RendererToBrowserFrame, SubframeToken, SubframesController,
};

#[cfg(any(test, feature = "browser_ui"))]
pub use registry::{renderer_process_count_for_test, renderer_process_spawn_count_for_test};
