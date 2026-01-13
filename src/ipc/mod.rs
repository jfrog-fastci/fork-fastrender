//! IPC types shared between the trusted browser process and the sandboxed renderer.
//!
//! Transport is a simple length-prefixed framing layer (see [`framing`]) with a hard maximum frame
//! size. Browser/renderer protocol types and validation live in [`protocol`].
//!
//! Low-level primitives like file-descriptor passing and shared-memory buffers are also defined in
//! this module so higher-level protocol layers can stay dependency-light.

pub mod ancillary;
pub mod error;
#[cfg(unix)]
pub mod fd_passing;
pub mod framing;
pub mod frame_pool;
pub mod network;
pub mod protocol;
pub mod shm;
pub mod websocket;

pub use error::IpcError;
pub use framing::{
  decode_bincode_payload, encode_bincode_payload, read_bincode_frame, read_frame, write_bincode_frame,
  write_frame, MAX_IPC_MESSAGE_BYTES,
};
pub use network::{NetworkToRenderer, RendererToNetwork};
