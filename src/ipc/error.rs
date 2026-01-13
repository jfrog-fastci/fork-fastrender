use thiserror::Error;

/// Errors produced by the IPC transport / protocol layers.
///
/// Security note: the browser must treat all IPC input as untrusted. In particular,
/// [`IpcError::ProtocolViolation`] and [`IpcError::MessageTooLarge`] indicate the peer sent a
/// malformed or policy-violating message.
#[derive(Debug, Error)]
pub enum IpcError {
  /// A transport-level operation exceeded its configured deadline.
  #[error("IPC operation timed out")]
  Timeout,

  /// The requested IPC operation is not supported on this platform/build.
  #[error("unsupported IPC operation: {msg}")]
  Unsupported { msg: String },

  /// An underlying OS I/O error that is *not* a clean disconnect.
  #[error("I/O error during IPC: {0}")]
  Io(#[source] std::io::Error),

  /// The peer disconnected (EOF, broken pipe, connection reset).
  #[error("IPC disconnected")]
  Disconnected,

  /// A length-prefixed frame declared a payload larger than the configured cap.
  #[error("IPC message too large: {len} bytes (max {max})")]
  MessageTooLarge { len: u32, max: u32 },

  /// The peer sent a well-formed frame that violates the expected protocol semantics.
  #[error("IPC protocol violation: {msg}")]
  ProtocolViolation { msg: String },

  /// Payload could not be deserialized.
  #[error("IPC deserialize error: {source}")]
  Deserialize {
    #[source]
    source: serde_json::Error,
  },

  /// Trusted-code bug: failed to serialize an outbound message.
  #[error("IPC serialize error: {source}")]
  Serialize {
    #[source]
    source: serde_json::Error,
  },

  /// Codec error (e.g. `bincode`) used by some IPC paths.
  #[error("IPC codec error: {source}")]
  Codec {
    #[source]
    source: Box<bincode::ErrorKind>,
  },

  /// The caller supplied invalid parameters (bug in trusted code, not peer input).
  #[error("invalid IPC parameters: {msg}")]
  InvalidParameters { msg: String },
}

impl From<std::io::Error> for IpcError {
  fn from(err: std::io::Error) -> Self {
    use std::io::ErrorKind;
    match err.kind() {
      ErrorKind::UnexpectedEof
      | ErrorKind::BrokenPipe
      | ErrorKind::ConnectionReset
      | ErrorKind::ConnectionAborted => Self::Disconnected,
      ErrorKind::TimedOut | ErrorKind::WouldBlock => Self::Timeout,
      _ => Self::Io(err),
    }
  }
}

impl From<bincode::Error> for IpcError {
  fn from(source: bincode::Error) -> Self {
    Self::Codec { source }
  }
}

#[cfg(test)]
mod tests {
  use super::IpcError;

  #[test]
  fn ipc_error_display_strings_are_stable() {
    assert_eq!(
      IpcError::InvalidParameters {
        msg: "oops".to_string()
      }
      .to_string(),
      "invalid IPC parameters: oops"
    );
    assert_eq!(
      IpcError::ProtocolViolation {
        msg: "bad message".to_string()
      }
      .to_string(),
      "IPC protocol violation: bad message"
    );
  }
}
