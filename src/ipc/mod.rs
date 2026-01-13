//! IPC types shared between the trusted browser process and the sandboxed renderer.
//!
//! Transport is a simple length-prefixed framing layer (see [`framing`]) with a hard maximum frame
//! size. Protocol types and validation live in [`protocol`].

pub mod error;
pub mod framing;
pub mod protocol;

pub use error::IpcError;
pub use framing::{read_frame, write_frame, MAX_IPC_MESSAGE_BYTES};
