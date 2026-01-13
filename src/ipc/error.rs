use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
  #[error("IPC stream ended unexpectedly")]
  UnexpectedEof,

  #[error("I/O error during IPC: {0}")]
  Io(#[source] std::io::Error),

  #[error("IPC protocol error: frame length was zero")]
  ZeroLength,

  #[error("IPC protocol error: frame length {len} exceeds maximum {max}")]
  FrameTooLarge { len: usize, max: usize },
}

impl From<std::io::Error> for IpcError {
  fn from(err: std::io::Error) -> Self {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
      Self::UnexpectedEof
    } else {
      Self::Io(err)
    }
  }
}
