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
pub mod connection;
pub mod error;
#[cfg(unix)]
pub mod fd_passing;
pub mod framing;
pub mod frame_pool;
pub mod network;
pub mod protocol;
pub mod shm;
pub mod websocket;

#[cfg(target_os = "linux")]
pub mod frame_slots;

pub use connection::IpcConnection;
pub use error::IpcError;
pub use framing::{
  decode_bincode_payload, encode_bincode_payload, read_bincode_frame, read_frame, write_bincode_frame,
  write_frame, MAX_IPC_MESSAGE_BYTES,
};
pub use network::{NetworkToRenderer, RendererToNetwork};
