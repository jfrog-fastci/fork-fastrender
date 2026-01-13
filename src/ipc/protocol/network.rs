//! Browser ↔ Network IPC protocol.
//!
//! This protocol is intended for a split-process architecture where the browser owns navigation
//! state and delegates actual network fetching to a (sandboxed) network process.
//!
//! # Cancellation semantics
//!
//! [`BrowserToNetwork::Cancel`] is **best-effort**:
//! - It may race with completion.
//! - If a completion (`FetchResult`) arrives first, the browser should ignore the later cancel
//!   acknowledgement (or lack thereof).
//! - If cancellation is observed first, the network process should abort the request and, if
//!   possible, close/abort any in-progress body transfer.
//!
//! The network process may reply with [`NetworkToBrowser::Cancelled`] when it successfully
//! cancels/aborts an in-flight request. Otherwise it may ignore the cancel (e.g. if the request is
//! already complete).

use serde::{Deserialize, Serialize};

use crate::ipc::IpcError;

/// Hard cap on URL byte length accepted by the browser ↔ network protocol.
///
/// This is an explicit guardrail in addition to the framing-layer decode limit
/// (`crate::ipc::MAX_IPC_MESSAGE_BYTES`): even if a hostile process stays under the overall frame
/// cap, we still want to bound individual string allocations.
pub const MAX_URL_BYTES: usize = 1024 * 1024;

/// Hard cap on cookie string byte length accepted by cookie-related IPC messages.
///
/// This applies to both:
/// - `StoreCookieFromDocument.cookie_string` (`document.cookie` setter input)
/// - `CookieHeader.value` (cookie header string returned by the network process)
pub const MAX_COOKIE_STRING_BYTES: usize = 4096;

// Compile-time guard: protocol-level per-field caps must fit under the framing-layer limit.
const _: () = {
  if MAX_URL_BYTES > crate::ipc::MAX_IPC_MESSAGE_BYTES {
    panic!("MAX_URL_BYTES must be <= MAX_IPC_MESSAGE_BYTES"); // fastrender-allow-panic
  }
  if MAX_COOKIE_STRING_BYTES > crate::ipc::MAX_IPC_MESSAGE_BYTES {
    panic!("MAX_COOKIE_STRING_BYTES must be <= MAX_IPC_MESSAGE_BYTES"); // fastrender-allow-panic
  }
};

/// Messages sent from the browser process to the network process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum BrowserToNetwork {
  /// Begin a fetch.
  ///
  /// Note: the exact request metadata will evolve. For now this is a minimal placeholder protocol
  /// sufficient to exercise cancellation semantics and `request_id` validation.
  Fetch { request_id: u64, url: String },

  /// Best-effort cancellation for an in-flight request.
  Cancel { request_id: u64 },

  /// Request the `Cookie` header value that would be sent for `url`.
  GetCookieHeader { request_id: u64, url: String },

  /// Store a cookie string as if it were set via `document.cookie` for `url`.
  StoreCookieFromDocument { url: String, cookie_string: String },
}

/// Messages sent from the network process back to the browser process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum NetworkToBrowser {
  /// Fetch completed (success or failure).
  FetchResult {
    request_id: u64,
    result: FetchResult,
  },

  /// The network process observed a cancellation request and aborted the fetch.
  Cancelled { request_id: u64 },

  /// Cookie header value for a request.
  ///
  /// - `Some("")` indicates cookie support is enabled but there are no cookies for the URL.
  /// - `None` indicates that the network process does not expose cookie state (unsupported).
  CookieHeader { request_id: u64, value: Option<String> },

  /// Optional acknowledgement for `StoreCookieFromDocument`.
  StoreCookieAck { ok: bool, error: Option<String> },
}

/// Outcome of a fetch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FetchResult {
  /// Successful response.
  ///
  /// # FD semantics
  /// In a future iteration, the response body may be transferred out-of-band via an accompanying FD
  /// (pipe/memfd). When that happens, `body_len` will describe the byte length of that transfer and
  /// [`NetworkToBrowser::expected_fds`] will return `1` for this variant.
  Ok { status: u16, body_len: u64 },

  /// Fetch failed.
  Err { message: String },
}

fn validate_request_id(request_id: u64) -> Result<(), IpcError> {
  if request_id == 0 {
    return Err(IpcError::ProtocolViolation {
      msg: "request_id must be non-zero".to_string(),
    });
  }
  Ok(())
}

fn validate_url(url: &str) -> Result<(), IpcError> {
  let len = url.len();
  if len > MAX_URL_BYTES {
    return Err(IpcError::ProtocolViolation {
      msg: format!("url too long: {len} bytes (max {MAX_URL_BYTES})"),
    });
  }
  Ok(())
}

fn validate_cookie_string(cookie_string: &str) -> Result<(), IpcError> {
  let len = cookie_string.len();
  if len > MAX_COOKIE_STRING_BYTES {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "cookie string too long: {len} bytes (max {MAX_COOKIE_STRING_BYTES})"
      ),
    });
  }
  Ok(())
}

impl BrowserToNetwork {
  /// Validate a browser → network message.
  pub fn validate(&self) -> Result<(), IpcError> {
    match self {
      BrowserToNetwork::Fetch { request_id, url } => {
        validate_request_id(*request_id)?;
        validate_url(url)
      }
      BrowserToNetwork::Cancel { request_id } => validate_request_id(*request_id),
      BrowserToNetwork::GetCookieHeader { request_id, url } => {
        validate_request_id(*request_id)?;
        validate_url(url)
      }
      BrowserToNetwork::StoreCookieFromDocument { url, cookie_string } => {
        validate_url(url)?;
        validate_cookie_string(cookie_string)
      }
    }
  }

  /// Number of file descriptors expected to accompany this message.
  ///
  /// (FDs are sent out-of-band via `SCM_RIGHTS` on platforms that support it.)
  pub fn expected_fds(&self) -> usize {
    match self {
      BrowserToNetwork::Fetch { .. }
      | BrowserToNetwork::Cancel { .. }
      | BrowserToNetwork::GetCookieHeader { .. }
      | BrowserToNetwork::StoreCookieFromDocument { .. } => 0,
    }
  }
}

impl NetworkToBrowser {
  /// Validate a network → browser message.
  pub fn validate(&self) -> Result<(), IpcError> {
    match self {
      NetworkToBrowser::FetchResult {
        request_id,
        result: _,
      } => validate_request_id(*request_id),
      NetworkToBrowser::Cancelled { request_id } => validate_request_id(*request_id),
      NetworkToBrowser::CookieHeader { request_id, value } => {
        validate_request_id(*request_id)?;
        if let Some(value) = value {
          validate_cookie_string(value)?;
        }
        Ok(())
      }
      NetworkToBrowser::StoreCookieAck { .. } => Ok(()),
    }
  }

  /// Number of file descriptors expected to accompany this message.
  ///
  /// (FDs are sent out-of-band via `SCM_RIGHTS` on platforms that support it.)
  pub fn expected_fds(&self) -> usize {
    match self {
      NetworkToBrowser::FetchResult { result, .. } => match result {
        FetchResult::Ok { .. } => 1,
        FetchResult::Err { .. } => 0,
      },
      NetworkToBrowser::Cancelled { .. }
      | NetworkToBrowser::CookieHeader { .. }
      | NetworkToBrowser::StoreCookieAck { .. } => 0,
    }
  }
}

#[cfg(test)]
mod cancel {
  use super::*;

  use std::sync::mpsc;
  use std::time::Duration;

  #[test]
  fn serialization_roundtrip_cancel() {
    let msg = BrowserToNetwork::Cancel { request_id: 42 };
    let json = serde_json::to_string(&msg).expect("serialize Cancel");
    let decoded: BrowserToNetwork = serde_json::from_str(&json).expect("deserialize Cancel");
    assert_eq!(decoded, msg);

    let msg = NetworkToBrowser::Cancelled { request_id: 42 };
    let json = serde_json::to_string(&msg).expect("serialize Cancelled");
    let decoded: NetworkToBrowser = serde_json::from_str(&json).expect("deserialize Cancelled");
    assert_eq!(decoded, msg);
  }

  #[test]
  fn expected_fds_counts() {
    assert_eq!(BrowserToNetwork::Cancel { request_id: 1 }.expected_fds(), 0);
    assert_eq!(
      NetworkToBrowser::Cancelled { request_id: 1 }.expected_fds(),
      0
    );

    // Ensure the helper isn't trivially always zero.
    assert_eq!(
      NetworkToBrowser::FetchResult {
        request_id: 1,
        result: FetchResult::Ok {
          status: 200,
          body_len: 0,
        },
      }
      .expected_fds(),
      1
    );
  }

  #[test]
  fn request_id_zero_rejected() {
    let err = BrowserToNetwork::Cancel { request_id: 0 }
      .validate()
      .expect_err("request_id=0 should be rejected");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));

    let err = NetworkToBrowser::Cancelled { request_id: 0 }
      .validate()
      .expect_err("request_id=0 should be rejected");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn cancel_before_complete_is_best_effort_and_prevents_fetch_result() {
    #[derive(Debug)]
    enum Cmd {
      Msg(BrowserToNetwork),
      Complete(u64),
      Shutdown,
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (resp_tx, resp_rx) = mpsc::channel::<NetworkToBrowser>();
    let (fetch_seen_tx, fetch_seen_rx) = mpsc::channel::<u64>();

    let handle = std::thread::spawn(move || {
      use std::collections::HashSet;
      let mut pending = HashSet::<u64>::new();

      while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
          Cmd::Msg(msg) => match msg {
            BrowserToNetwork::Fetch { request_id, .. } => {
              // Assume request_id has already been validated by the sender.
              pending.insert(request_id);
              let _ = fetch_seen_tx.send(request_id);
            }
            BrowserToNetwork::Cancel { request_id } => {
              if pending.remove(&request_id) {
                let _ = resp_tx.send(NetworkToBrowser::Cancelled { request_id });
              }
            }
            BrowserToNetwork::GetCookieHeader { .. }
            | BrowserToNetwork::StoreCookieFromDocument { .. } => {
              // Not exercised by the cancellation tests.
            }
          },

          Cmd::Complete(request_id) => {
            if pending.remove(&request_id) {
              let _ = resp_tx.send(NetworkToBrowser::FetchResult {
                request_id,
                result: FetchResult::Ok {
                  status: 200,
                  body_len: 0,
                },
              });
            }
          }

          Cmd::Shutdown => break,
        }
      }
    });

    let request_id = 1u64;
    cmd_tx
      .send(Cmd::Msg(BrowserToNetwork::Fetch {
        request_id,
        url: "https://example.test/".to_string(),
      }))
      .unwrap();
    assert_eq!(
      fetch_seen_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
      request_id
    );

    // Cancel before allowing the mock network loop to complete the request.
    cmd_tx
      .send(Cmd::Msg(BrowserToNetwork::Cancel { request_id }))
      .unwrap();
    cmd_tx.send(Cmd::Complete(request_id)).unwrap();

    // We should observe cancellation, and no completion should follow.
    let msg = resp_rx
      .recv_timeout(Duration::from_secs(1))
      .expect("expected a response");
    assert_eq!(msg, NetworkToBrowser::Cancelled { request_id });

    let second = resp_rx.recv_timeout(Duration::from_millis(50));
    assert!(
      second.is_err(),
      "unexpected second message after Cancelled: {second:?}"
    );

    let _ = cmd_tx.send(Cmd::Shutdown);
    handle.join().expect("join mock network loop");
  }
}

#[cfg(test)]
mod cookies {
  use super::*;

  #[test]
  fn serialization_roundtrip() {
    let msg = BrowserToNetwork::GetCookieHeader {
      request_id: 42,
      url: "https://example.com/".to_string(),
    };
    let json = serde_json::to_string(&msg).expect("serialize GetCookieHeader");
    let decoded: BrowserToNetwork = serde_json::from_str(&json).expect("deserialize GetCookieHeader");
    assert_eq!(decoded, msg);

    let msg = NetworkToBrowser::CookieHeader {
      request_id: 42,
      value: Some("a=b".to_string()),
    };
    let json = serde_json::to_string(&msg).expect("serialize CookieHeader");
    let decoded: NetworkToBrowser = serde_json::from_str(&json).expect("deserialize CookieHeader");
    assert_eq!(decoded, msg);
  }

  #[test]
  fn validator_rejects_oversized_cookie_strings() {
    let msg = BrowserToNetwork::StoreCookieFromDocument {
      url: "https://example.com/".to_string(),
      cookie_string: "a=".to_string() + &"x".repeat(MAX_COOKIE_STRING_BYTES),
    };
    let err = msg
      .validate()
      .expect_err("expected oversized cookie_string to be rejected");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }
}
