//! IPC types shared between the trusted browser process and the sandboxed renderer.
//!
//! Transport is a simple length-prefixed framing layer (see [`framing`]) with a hard maximum frame
//! size. Browser/renderer protocol types and validation live in [`protocol`].
//!
//! Low-level primitives like file-descriptor passing and shared-memory buffers are also defined in
//! this module so higher-level protocol layers can stay dependency-light.
//!
//! Linux-only: [`frame_slots`] provides a test-only reference implementation for a "browser
//! allocates SHM slots once, subsequent messages are control-only" transport.

pub mod ancillary;
pub mod cancel;
pub mod connection;
pub mod error;
pub mod framing;
pub mod framed_codec;
pub mod frame_pool;
pub mod network;
pub mod network_service;
pub mod pixels;
pub mod limits;
pub mod protocol;
pub mod received_frame;
pub mod session;
pub mod shm;
pub mod transport;
#[cfg(unix)]
pub mod validate;
pub mod sync;
pub mod types;
pub mod websocket;
pub mod shmem;

#[cfg(unix)]
pub mod bootstrap;
#[cfg(unix)]
pub mod fd_passing;

#[cfg(target_os = "linux")]
pub mod frame_slots;
#[cfg(target_os = "linux")]
pub mod unix_seqpacket;

#[cfg(test)]
mod platform_compile;

pub use connection::IpcConnection;
pub use error::IpcError;
pub use framing::{
  decode_bincode_payload, decode_bincode_payload_with_limit, encode_bincode_payload,
  encode_bincode_payload_with_limit, read_bincode_frame, read_frame, read_frame_with_max,
  write_bincode_frame, write_frame, write_frame_with_max,
};
pub use limits::MAX_IPC_MESSAGE_BYTES;
pub use network::{NetworkToRenderer, RendererToNetwork};
pub use network_service::IpcFetchServer;
pub use received_frame::{FrameMeta, ReceivedFrame, ShmemSliceView};
pub use types::{PointF32, RectF32, ScrollMetricsIpc, ScrollStateIpc};

#[cfg(target_os = "linux")]
pub mod shared_memory;

#[cfg(target_os = "linux")]
pub use shared_memory::{MmapView, MmapViewMut, SharedMemory, SharedMemoryError};
