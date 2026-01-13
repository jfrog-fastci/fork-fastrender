pub mod error;
pub mod framing;

pub use error::IpcError;
pub use framing::{read_frame, write_frame, MAX_IPC_MESSAGE_BYTES};
