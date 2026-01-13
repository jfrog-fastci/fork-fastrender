use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
  #[error("IPC operation timed out")]
  Timeout,

  #[error("unsupported IPC operation: {message}")]
  Unsupported { message: String },

  #[error("IPC stream ended unexpectedly")]
  UnexpectedEof,

  #[error("I/O error during IPC: {0}")]
  Io(#[source] std::io::Error),

  /// Caller-provided parameters were invalid (e.g. impossible viewport or buffer sizes).
  ///
  /// This is used by IPC helpers that are shared between the trusted browser and untrusted
  /// renderer, where some errors are not strictly "protocol validation" failures but still need
  /// to surface a descriptive message.
  #[error("invalid IPC parameters: {message}")]
  InvalidParameters { message: String },

  /// The remote side violated an agreed-upon invariant (e.g. inconsistent shared memory mapping).
  #[error("IPC protocol violation: {message}")]
  ProtocolViolation { message: String },

  #[error("IPC protocol error: frame length was zero")]
  ZeroLength,

  #[error("IPC protocol error: frame length {len} exceeds maximum {max}")]
  FrameTooLarge { len: usize, max: usize },

  // ==========================================================================
  // Generic errors (used by helper modules like the shared frame pool)
  // ==========================================================================
  #[error("IPC codec error")]
  Codec(#[source] Box<bincode::ErrorKind>),

  #[error("failed to serialize IPC JSON message: {0}")]
  Serialize(#[source] serde_json::Error),

  #[error("failed to deserialize IPC JSON message: {0}")]
  Deserialize(#[source] serde_json::Error),
  // ==========================================================================
  // Protocol validation errors (renderer → browser)
  // ==========================================================================

  #[error("request_id must be non-zero")]
  RequestIdZero,

  #[error("url too long: {len} bytes (max {max})")]
  UrlTooLong { len: usize, max: usize },

  #[error("cookie string too long: {len} bytes (max {max})")]
  CookieStringTooLong { len: usize, max: usize },

  #[error("frame buffer list too large: {len} (max {max})")]
  TooManyFrameBuffers { len: usize, max: usize },

  #[error("shared memory id is empty")]
  EmptyId,

  #[error("shared memory id too long: {len} (max {max})")]
  IdTooLong { len: usize, max: usize },

  #[error("frame buffer byte_len must be non-zero")]
  FrameBufferByteLenZero,

  #[error("frame buffer stride_bytes must be non-zero")]
  FrameBufferStrideZero,

  #[error("frame buffer max_width_px/max_height_px must be non-zero")]
  FrameBufferMaxDimensionsZero,

  #[error(
    "frame buffer stride_bytes={stride_bytes} is smaller than min_row_bytes={min_row_bytes}"
  )]
  FrameBufferStrideTooSmall {
    stride_bytes: usize,
    min_row_bytes: usize,
  },

  #[error("frame buffer backing store too small: required={required_bytes} available={byte_len}")]
  FrameBufferTooSmall {
    required_bytes: usize,
    byte_len: usize,
  },

  #[error("protocol version mismatch: got {got}, expected {expected}")]
  ProtocolVersionMismatch { got: u32, expected: u32 },

  #[error("generation mismatch: got {got}, expected {expected}")]
  GenerationMismatch { got: u64, expected: u64 },

  #[error("buffer_index {buffer_index} out of range (buffer_count={buffer_count})")]
  InvalidBufferIndex {
    buffer_index: u32,
    buffer_count: usize,
  },

  #[error("frame dimensions must be non-zero (width_px={width_px}, height_px={height_px})")]
  FrameDimensionsZero { width_px: u32, height_px: u32 },

  #[error(
    "frame dimensions exceed negotiated maximums: {width_px}x{height_px} > {max_width_px}x{max_height_px}"
  )]
  FrameDimensionsExceedMax {
    width_px: u32,
    height_px: u32,
    max_width_px: u32,
    max_height_px: u32,
  },

  #[error("frame row bytes {row_bytes} exceed stride_bytes {stride_bytes}")]
  FrameRowBytesExceedStride {
    row_bytes: usize,
    stride_bytes: usize,
  },

  #[error("frame exceeds shared memory buffer: required={required_bytes} available={byte_len}")]
  FrameExceedsBufferLen {
    required_bytes: usize,
    byte_len: usize,
  },

  #[error("invalid device pixel ratio {dpr}")]
  InvalidDpr { dpr: f32 },

  #[error("crash reason too long: {len} (max {max})")]
  CrashReasonTooLong { len: usize, max: usize },

  // ==========================================================================
  // Protocol validation errors (browser → renderer)
  // ==========================================================================

  #[error("shutdown reason too long: {len} (max {max})")]
  ShutdownReasonTooLong { len: usize, max: usize },

  #[error("too many files: {len} (max {max})")]
  TooManyFiles { len: usize, max: usize },

  #[error("file name too long: {len} bytes (max {max})")]
  FileNameTooLong { len: usize, max: usize },

  #[error("file name must be a basename (no path separators): {name:?}")]
  FileNameNotBasename { name: String },

  #[error("total file size metadata too large: {total} bytes (max {max})")]
  TotalFileSizeTooLarge { total: u64, max: u64 },

  #[error("arithmetic overflow while validating IPC message")]
  ArithmeticOverflow,
}

impl From<std::io::Error> for IpcError {
  fn from(err: std::io::Error) -> Self {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
      Self::UnexpectedEof
    } else if matches!(
      err.kind(),
      std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
      Self::Timeout
    } else {
      Self::Io(err)
    }
  }
}

impl From<bincode::Error> for IpcError {
  fn from(err: bincode::Error) -> Self {
    Self::Codec(err)
  }
}

#[cfg(test)]
mod tests {
  use super::IpcError;

  #[test]
  fn ipc_error_display_strings_are_stable() {
    assert_eq!(
      IpcError::InvalidParameters {
        message: "oops".to_string()
      }
      .to_string(),
      "invalid IPC parameters: oops"
    );
    assert_eq!(
      IpcError::ProtocolViolation {
        message: "bad message".to_string()
      }
      .to_string(),
      "IPC protocol violation: bad message"
    );
  }
}
