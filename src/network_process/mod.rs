//! Components intended to run in a dedicated network process.
//!
//! The multiprocess architecture is still under construction, but we keep network-facing state
//! (WebSockets, HTTP fetch, etc.) behind explicit managers so we can apply hard resource caps even
//! when the renderer is compromised.

pub mod websocket_manager;
#[cfg(feature = "direct_websocket")]
pub mod websocket_runtime;

#[doc(hidden)]
pub mod ipc;

mod client;

pub use client::{
  spawn_network_process, try_spawn_network_process, DownloadClient, IpcResourceFetcher,
  NetworkClient, NetworkProcessConfig, NetworkProcessHandle, WebSocketBackend, WebSocketMessage,
  WebSocketStream,
};
