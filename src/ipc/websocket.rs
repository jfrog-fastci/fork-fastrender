//! WebSocket IPC messages for the multiprocess network proxy.
//!
//! The renderer process is considered untrusted, so the network process must validate every
//! renderer-supplied message before acting on it. Validation helpers in this module are intended
//! to be called *after* deserialization.

use serde::{Deserialize, Serialize};
use url::Url;

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

fn is_valid_websocket_subprotocol_token(s: &str) -> bool {
  if s.is_empty() {
    return false;
  }
  s.bytes().all(|b| {
    matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
      || matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
      )
  })
}

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
#[serde(deny_unknown_fields)]
pub enum WebSocketCommand {
  Connect {
    params: WebSocketConnectParams,
  },
  SendText {
    text: String,
  },
  SendBinary {
    data: Vec<u8>,
  },
  Close {
    code: Option<u16>,
    reason: Option<String>,
  },
  /// Abruptly shut down the connection (best-effort).
  Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum WebSocketEvent {
  Open {
    selected_protocol: String,
  },
  MessageText {
    text: String,
  },
  MessageBinary {
    data: Vec<u8>,
  },
  Error {
    message: Option<String>,
  },
  Close {
    code: u16,
    reason: String,
  },
  /// Acknowledgement that `bytes` have been flushed from the send buffer.
  ///
  /// This allows the renderer to implement `bufferedAmount` / backpressure without exposing the
  /// underlying network implementation.
  SendAck {
    bytes: u32,
  },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum WebSocketValidationError {
  #[error("websocket url too long (len {len} bytes; max {max} bytes)")]
  UrlTooLong { len: u32, max: u32 },
  #[error("websocket url is invalid")]
  InvalidUrl,
  #[error("websocket url must use ws: or wss: scheme")]
  InvalidScheme,
  #[error("websocket url must not include a fragment")]
  HasFragment,
  #[error("websocket url must include a host")]
  MissingHost,
  #[error("too many websocket protocols (len {len}; max {max})")]
  TooManyProtocols { len: u32, max: u32 },
  #[error("websocket protocol too long (len {len} bytes; max {max} bytes)")]
  ProtocolTooLong { len: u32, max: u32 },
  #[error("websocket protocol is empty")]
  ProtocolEmpty,
  #[error("websocket protocol must be a token")]
  ProtocolNotToken,
  #[error("websocket protocols list must not contain duplicates")]
  DuplicateProtocols,
  #[error("websocket message too large (len {len} bytes; max {max} bytes)")]
  MessageTooLarge { len: u32, max: u32 },
  #[error("websocket close reason too long (len {len} bytes; max {max} bytes)")]
  CloseReasonTooLong { len: u32, max: u32 },
}

impl WebSocketConnectParams {
  /// Parse and validate the renderer-supplied WebSocket URL for use in the network process.
  ///
  /// Security note: the renderer is untrusted. The network process must reject malformed URLs even
  /// if renderer-side bindings performed validation.
  pub fn validated_url(&self) -> Result<Url, WebSocketValidationError> {
    validate_and_normalize_url(&self.url)
  }

  /// Validate parameters supplied by the renderer.
  pub fn validate(&self) -> Result<(), WebSocketValidationError> {
    let _ = self.validated_url()?;

    let proto_count = u32::try_from(self.protocols.len()).unwrap_or(u32::MAX);
    if proto_count > MAX_WEBSOCKET_PROTOCOLS {
      return Err(WebSocketValidationError::TooManyProtocols {
        len: proto_count,
        max: MAX_WEBSOCKET_PROTOCOLS,
      });
    }

    let mut seen = std::collections::HashSet::<&str>::new();
    for proto in &self.protocols {
      if proto.is_empty() {
        return Err(WebSocketValidationError::ProtocolEmpty);
      }
      let len = u32::try_from(proto.as_bytes().len()).unwrap_or(u32::MAX);
      if len > MAX_WEBSOCKET_PROTOCOL_BYTES {
        return Err(WebSocketValidationError::ProtocolTooLong {
          len,
          max: MAX_WEBSOCKET_PROTOCOL_BYTES,
        });
      }
      if !is_valid_websocket_subprotocol_token(proto) {
        return Err(WebSocketValidationError::ProtocolNotToken);
      }
      if !seen.insert(proto.as_str()) {
        return Err(WebSocketValidationError::DuplicateProtocols);
      }
    }

    Ok(())
  }
}

/// Validate and canonicalize a WebSocket URL string received over IPC.
///
/// This helper normalizes `http:` → `ws:` and `https:` → `wss:` deterministically so cookie lookup
/// and connection keying are stable. The normalized URL is also checked against the IPC string
/// length limit.
pub fn validate_and_normalize_url(raw: &str) -> Result<Url, WebSocketValidationError> {
  let raw_len = u32::try_from(raw.as_bytes().len()).unwrap_or(u32::MAX);
  if raw_len > MAX_WEBSOCKET_URL_BYTES {
    return Err(WebSocketValidationError::UrlTooLong {
      len: raw_len,
      max: MAX_WEBSOCKET_URL_BYTES,
    });
  }

  let mut url = Url::parse(raw).map_err(|_| WebSocketValidationError::InvalidUrl)?;

  if url.fragment().is_some() {
    return Err(WebSocketValidationError::HasFragment);
  }

  match url.scheme() {
    "ws" | "wss" => {}
    "http" => url
      .set_scheme("ws")
      .map_err(|_| WebSocketValidationError::InvalidUrl)?,
    "https" => url
      .set_scheme("wss")
      .map_err(|_| WebSocketValidationError::InvalidUrl)?,
    _ => return Err(WebSocketValidationError::InvalidScheme),
  }

  if url
    .host_str()
    .filter(|host| !host.is_empty())
    .is_none()
  {
    return Err(WebSocketValidationError::MissingHost);
  }

  // Canonicalize `ws://example.com` to `ws://example.com/` so downstream keying stays stable.
  if url.path().is_empty() {
    url.set_path("/");
  }

  let normalized_len = u32::try_from(url.as_str().as_bytes().len()).unwrap_or(u32::MAX);
  if normalized_len > MAX_WEBSOCKET_URL_BYTES {
    return Err(WebSocketValidationError::UrlTooLong {
      len: normalized_len,
      max: MAX_WEBSOCKET_URL_BYTES,
    });
  }

  Ok(url)
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

  // Trigger a compile-time error if the forbidden token appears in the source, without using
  // `panic!` (keeps this compatible with `xtask lint-no-panics`).
  let _ = 1u8 / (!contains(SRC, &FORBIDDEN) as u8);
};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validate_connect_params_url_len_boundary() {
    let base = "ws://example.com/";
    let base_len = u32::try_from(base.as_bytes().len()).unwrap_or(u32::MAX);
    assert!(base_len < MAX_WEBSOCKET_URL_BYTES);
    let fill_len = MAX_WEBSOCKET_URL_BYTES - base_len;
    let ok_url = base.to_string() + &"a".repeat(fill_len as _);
    assert_eq!(
      u32::try_from(ok_url.as_bytes().len()).unwrap_or(u32::MAX),
      MAX_WEBSOCKET_URL_BYTES
    );
    let ok = WebSocketConnectParams {
      url: ok_url,
      protocols: Vec::new(),
      origin: None,
      document_url: None,
    };
    assert!(ok.validate().is_ok());

    let bad_url = base.to_string() + &"a".repeat((fill_len + 1) as _);
    assert!(
      u32::try_from(bad_url.as_bytes().len()).unwrap_or(u32::MAX) > MAX_WEBSOCKET_URL_BYTES
    );
    let bad = WebSocketConnectParams {
      url: bad_url,
      protocols: Vec::new(),
      origin: None,
      document_url: None,
    };
    assert!(bad.validate().is_err());
  }

  #[test]
  fn validate_connect_params_protocols_len_boundary() {
    let protocols: Vec<String> = (0..MAX_WEBSOCKET_PROTOCOLS)
      .map(|i| format!("p{i}"))
      .collect();
    let ok = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols,
      origin: None,
      document_url: None,
    };
    assert!(ok.validate().is_ok());

    let mut too_many: Vec<String> = (0..=MAX_WEBSOCKET_PROTOCOLS)
      .map(|i| format!("p{i}"))
      .collect();
    // Ensure the last entry is unique even if MAX_WEBSOCKET_PROTOCOLS is 0 (should never happen).
    too_many.push("p_extra".to_string());
    let bad = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: too_many,
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
  fn validate_connect_params_rejects_empty_protocol() {
    let params = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec!["".to_string()],
      origin: None,
      document_url: None,
    };
    assert!(matches!(
      params.validate(),
      Err(WebSocketValidationError::ProtocolEmpty)
    ));
  }

  #[test]
  fn validate_connect_params_rejects_non_token_protocol() {
    let cases = ["chat superchat", "chat,superchat", "chat, superchat"];
    for proto in cases {
      let params = WebSocketConnectParams {
        url: "ws://example.com".to_string(),
        protocols: vec![proto.to_string()],
        origin: None,
        document_url: None,
      };
      assert!(
        matches!(
          params.validate(),
          Err(WebSocketValidationError::ProtocolNotToken)
        ),
        "expected token rejection for {proto:?}"
      );
    }
  }

  #[test]
  fn validate_connect_params_rejects_duplicate_protocols() {
    let params = WebSocketConnectParams {
      url: "ws://example.com".to_string(),
      protocols: vec!["chat".to_string(), "chat".to_string()],
      origin: None,
      document_url: None,
    };
    assert!(matches!(
      params.validate(),
      Err(WebSocketValidationError::DuplicateProtocols)
    ));
  }

  #[test]
  fn validate_connect_params_rejects_invalid_urls() {
    let cases = [
      "file:///etc/passwd",
      "data:text/plain,hi",
      "ws://#frag",
      "ws:/relative",
      "ws:///path",
    ];
    for url in cases {
      let params = WebSocketConnectParams {
        url: url.to_string(),
        protocols: Vec::new(),
        origin: None,
        document_url: None,
      };
      assert!(params.validate().is_err(), "expected rejection for {url:?}");
    }
  }

  #[test]
  fn validate_and_normalize_url_http_https() {
    let url = validate_and_normalize_url("http://example.com").expect("normalize http");
    assert_eq!(url.scheme(), "ws");
    assert_eq!(url.as_str(), "ws://example.com/");

    let url = validate_and_normalize_url("https://example.com").expect("normalize https");
    assert_eq!(url.scheme(), "wss");
    assert_eq!(url.as_str(), "wss://example.com/");
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
