//! WebSocket IPC messages for the multiprocess network proxy.
//!
//! The renderer process is considered untrusted, so the network process must validate every
//! renderer-supplied message before acting on it. Validation helpers in this module are intended
//! to be called *after* deserialization.

use serde::{Deserialize, Serialize};

/// Maximum allowed UTF-8 byte length of a WebSocket URL sent over IPC.
pub const MAX_WEBSOCKET_URL_BYTES: u32 = 8 * 1024;
/// Maximum number of subprotocols allowed in a single connect request.
pub const MAX_WEBSOCKET_PROTOCOLS: u32 = 32;
/// Maximum allowed UTF-8 byte length of any single subprotocol string.
pub const MAX_WEBSOCKET_PROTOCOL_BYTES: u32 = 1 * 1024;
/// Maximum allowed payload size for a single SendText/SendBinary message.
pub const MAX_WEBSOCKET_MESSAGE_BYTES: u32 = 4 * 1024 * 1024;
/// Maximum allowed UTF-8 byte length of a close reason string.
///
/// This matches the WebSocket API limitation (123 bytes).
pub const MAX_WEBSOCKET_CLOSE_REASON_BYTES: u32 = 123;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebSocketConnectParams {
  pub url: String,
  /// Requested subprotocols (e.g. `["graphql-ws"]`).
  pub protocols: Vec<String>,
  /// The `Origin` value that should be used for the handshake, if any.
  ///
  /// This is derived from the initiator's origin in the renderer process. The network process must
  /// validate it against the supplied document context and its own policy decisions.
  pub origin: Option<String>,
  /// URL of the document (or worker script) that initiated the WebSocket.
  ///
  /// Used for cookie/origin enforcement in the network process.
  pub document_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WebSocketCommand {
  Connect { params: WebSocketConnectParams },
  SendText { text: String },
  SendBinary { data: Vec<u8> },
  Close { code: Option<u16>, reason: Option<String> },
  /// Abruptly shut down the connection (best-effort).
  Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WebSocketEvent {
  Open { selected_protocol: String },
  MessageText { text: String },
  MessageBinary { data: Vec<u8> },
  Error { message: Option<String> },
  Close { code: u16, reason: String },
  /// Acknowledgement that `bytes` have been flushed from the send buffer.
  ///
  /// This allows the renderer to implement `bufferedAmount` / backpressure without exposing the
  /// underlying network implementation.
  SendAck { bytes: u32 },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum WebSocketValidationError {
  #[error("websocket url too long (len {len} bytes; max {max} bytes)")]
  UrlTooLong { len: u32, max: u32 },
  #[error("too many websocket protocols (len {len}; max {max})")]
  TooManyProtocols { len: u32, max: u32 },
  #[error("websocket protocol too long (len {len} bytes; max {max} bytes)")]
  ProtocolTooLong { len: u32, max: u32 },
  #[error("websocket message too large (len {len} bytes; max {max} bytes)")]
  MessageTooLarge { len: u32, max: u32 },
  #[error("websocket close reason too long (len {len} bytes; max {max} bytes)")]
  CloseReasonTooLong { len: u32, max: u32 },
}

impl WebSocketConnectParams {
  /// Validate parameters supplied by the renderer.
  pub fn validate(&self) -> Result<(), WebSocketValidationError> {
    let url_len = u32::try_from(self.url.as_bytes().len()).unwrap_or(u32::MAX);
    if url_len > MAX_WEBSOCKET_URL_BYTES {
      return Err(WebSocketValidationError::UrlTooLong {
        len: url_len,
        max: MAX_WEBSOCKET_URL_BYTES,
      });
    }

    let proto_count = u32::try_from(self.protocols.len()).unwrap_or(u32::MAX);
    if proto_count > MAX_WEBSOCKET_PROTOCOLS {
      return Err(WebSocketValidationError::TooManyProtocols {
        len: proto_count,
        max: MAX_WEBSOCKET_PROTOCOLS,
      });
    }

    for proto in &self.protocols {
      let len = u32::try_from(proto.as_bytes().len()).unwrap_or(u32::MAX);
      if len > MAX_WEBSOCKET_PROTOCOL_BYTES {
        return Err(WebSocketValidationError::ProtocolTooLong {
          len,
          max: MAX_WEBSOCKET_PROTOCOL_BYTES,
        });
      }
    }

    Ok(())
  }
}

impl WebSocketCommand {
  /// Validate a command supplied by the renderer.
  pub fn validate(&self) -> Result<(), WebSocketValidationError> {
    match self {
      Self::Connect { params } => params.validate(),
      Self::SendText { text } => {
        let len = u32::try_from(text.as_bytes().len()).unwrap_or(u32::MAX);
        if len > MAX_WEBSOCKET_MESSAGE_BYTES {
          return Err(WebSocketValidationError::MessageTooLarge {
            len,
            max: MAX_WEBSOCKET_MESSAGE_BYTES,
          });
        }
        Ok(())
      }
      Self::SendBinary { data } => {
        let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        if len > MAX_WEBSOCKET_MESSAGE_BYTES {
          return Err(WebSocketValidationError::MessageTooLarge {
            len,
            max: MAX_WEBSOCKET_MESSAGE_BYTES,
          });
        }
        Ok(())
      }
      Self::Close { reason, .. } => {
        if let Some(reason) = reason.as_deref() {
          let len = u32::try_from(reason.as_bytes().len()).unwrap_or(u32::MAX);
          if len > MAX_WEBSOCKET_CLOSE_REASON_BYTES {
            return Err(WebSocketValidationError::CloseReasonTooLong {
              len,
              max: MAX_WEBSOCKET_CLOSE_REASON_BYTES,
            });
          }
        }
        Ok(())
      }
      Self::Shutdown => Ok(()),
    }
  }

  /// Normalizes a close code to a value that is safe to send in a close frame.
  ///
  /// Renderer-supplied close codes must be treated as untrusted. Invalid codes are mapped to
  /// `1000` (normal closure).
  ///
  /// This is intentionally forgiving: the network process can still honor the close request without
  /// risking a protocol/library error.
  pub fn normalized_close_code(code: Option<u16>) -> u16 {
    let code = code.unwrap_or(1000);
    if is_valid_close_code(code) {
      code
    } else {
      1000
    }
  }
}

/// Returns true if `code` is valid to include in a close frame per RFC 6455.
///
/// Note: This intentionally *excludes* codes reserved for internal use (1005, 1006, 1015), which
/// may appear in close events but must not be sent on the wire.
pub fn is_valid_close_code(code: u16) -> bool {
  match code {
    1000 | 1001 | 1002 | 1003 => true,
    1004 | 1005 | 1006 => false,
    1007..=1014 => true,
    1015 => false,
    3000..=4999 => true,
    _ => false,
  }
}

// Compile-time guard: this module must not mention pointer-sized integer tokens.
const _: () = {
  const SRC: &[u8] = include_bytes!("websocket.rs");
  const FORBIDDEN: [u8; 5] = [0x75, 0x73, 0x69, 0x7a, 0x65];

  const fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
      return false;
    }
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
      let mut j = 0;
      while j < needle.len() {
        if haystack[i + j] != needle[j] {
          break;
        }
        j += 1;
      }
      if j == needle.len() {
        return true;
      }
      i += 1;
    }
    false
  }

  if contains(SRC, &FORBIDDEN) {
    panic!("websocket IPC module contains a forbidden integer token");
  }
};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validate_connect_params_url_len_boundary() {
    let ok = WebSocketConnectParams {
      url: "a".repeat(MAX_WEBSOCKET_URL_BYTES as _),
      protocols: Vec::new(),
      origin: None,
      document_url: None,
    };
    assert!(ok.validate().is_ok());

    let bad = WebSocketConnectParams {
      url: "a".repeat((MAX_WEBSOCKET_URL_BYTES + 1) as _),
      protocols: Vec::new(),
      origin: None,
      document_url: None,
    };
    assert!(bad.validate().is_err());
  }

  #[test]
  fn validate_connect_params_protocols_len_boundary() {
    let proto = "p".to_string();
    let ok = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec![proto.clone(); MAX_WEBSOCKET_PROTOCOLS as _],
      origin: None,
      document_url: None,
    };
    assert!(ok.validate().is_ok());

    let bad = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec![proto; (MAX_WEBSOCKET_PROTOCOLS + 1) as _],
      origin: None,
      document_url: None,
    };
    assert!(bad.validate().is_err());
  }

  #[test]
  fn validate_connect_params_protocol_len_boundary() {
    let ok = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec!["p".repeat(MAX_WEBSOCKET_PROTOCOL_BYTES as _)],
      origin: None,
      document_url: None,
    };
    assert!(ok.validate().is_ok());

    let bad = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec!["p".repeat((MAX_WEBSOCKET_PROTOCOL_BYTES + 1) as _)],
      origin: None,
      document_url: None,
    };
    assert!(bad.validate().is_err());
  }

  #[test]
  fn validate_command_message_size_boundary() {
    let ok = WebSocketCommand::SendText {
      text: "a".repeat(MAX_WEBSOCKET_MESSAGE_BYTES as _),
    };
    assert!(ok.validate().is_ok());

    let bad = WebSocketCommand::SendText {
      text: "a".repeat((MAX_WEBSOCKET_MESSAGE_BYTES + 1) as _),
    };
    assert!(bad.validate().is_err());

    let ok_bin = WebSocketCommand::SendBinary {
      data: vec![0u8; MAX_WEBSOCKET_MESSAGE_BYTES as _],
    };
    assert!(ok_bin.validate().is_ok());

    let bad_bin = WebSocketCommand::SendBinary {
      data: vec![0u8; (MAX_WEBSOCKET_MESSAGE_BYTES + 1) as _],
    };
    assert!(bad_bin.validate().is_err());
  }

  #[test]
  fn validate_command_close_reason_boundary() {
    let ok = WebSocketCommand::Close {
      code: Some(1000),
      reason: Some("a".repeat(MAX_WEBSOCKET_CLOSE_REASON_BYTES as _)),
    };
    assert!(ok.validate().is_ok());

    let bad = WebSocketCommand::Close {
      code: Some(1000),
      reason: Some("a".repeat((MAX_WEBSOCKET_CLOSE_REASON_BYTES + 1) as _)),
    };
    assert!(bad.validate().is_err());
  }

  #[test]
  fn normalize_close_code_rfc6455() {
    // Valid codes are preserved.
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1000)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1007)), 1007);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(3000)), 3000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(4999)), 4999);

    // Invalid/reserved codes are normalized.
    assert_eq!(WebSocketCommand::normalized_close_code(None), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(0)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(999)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1004)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1005)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1006)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(1015)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(2000)), 1000);
    assert_eq!(WebSocketCommand::normalized_close_code(Some(5000)), 1000);
  }
}
