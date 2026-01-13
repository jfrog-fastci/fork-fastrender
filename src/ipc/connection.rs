//! JSON IPC connection built on top of the framing layer.
//!
//! This module provides:
//! - [`IpcConnection`], a small wrapper that sends/receives length-prefixed JSON messages while
//!   enforcing [`crate::ipc::limits::MAX_IPC_MESSAGE_BYTES`] (both before sending and before
//!   decoding).
//! - In-memory decode helpers that operate on borrowed byte slices (useful for fuzzing and other
//!   non-streaming contexts).

use super::error::IpcError;
use super::framing::{self, read_frame, write_frame};
use super::limits::MAX_IPC_MESSAGE_BYTES;
use super::protocol;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{Read, Write};

/// Length-delimited IPC connection that encodes payloads as JSON.
pub struct IpcConnection<R, W> {
  reader: R,
  writer: W,
}

impl<R, W> IpcConnection<R, W> {
  pub fn new(reader: R, writer: W) -> Self {
    Self { reader, writer }
  }

  pub fn into_inner(self) -> (R, W) {
    (self.reader, self.writer)
  }

  pub fn reader_mut(&mut self) -> &mut R {
    &mut self.reader
  }

  pub fn writer_mut(&mut self) -> &mut W {
    &mut self.writer
  }
}

impl<R: Read, W: Write> IpcConnection<R, W> {
  pub fn send_json<T: Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(msg).map_err(IpcError::Serialize)?;

    if payload.len() > MAX_IPC_MESSAGE_BYTES {
      return Err(IpcError::FrameTooLarge {
        len: payload.len(),
        max: MAX_IPC_MESSAGE_BYTES,
      });
    }

    write_frame(&mut self.writer, &payload)
  }

  pub fn recv_json<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
    // Security note: the JSON IPC boundary is treated as untrusted (e.g. renderer → browser or
    // renderer → network-process).
    //
    // We deserialize via `serde_json::Value` first so serde_json's recursion limit applies to the
    // entire payload, including deeply nested structures hidden in unknown fields.
    //
    // IPC protocol structs are also annotated with `#[serde(deny_unknown_fields)]` so we fail
    // closed if an unexpected field appears.
    let payload = read_frame(&mut self.reader)?;
    let value: serde_json::Value = serde_json::from_slice(&payload).map_err(IpcError::Deserialize)?;
    serde_json::from_value(value).map_err(IpcError::Deserialize)
  }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
  #[error(transparent)]
  Frame(#[from] IpcError),

  #[error(transparent)]
  Json(#[from] serde_json::Error),
}

/// Decode a length-prefixed frame from `data` and deserialize it as a
/// [`protocol::renderer::RendererToBrowser`] message.
pub fn decode_renderer_to_browser_from_bytes(
  data: &[u8],
) -> Result<protocol::renderer::RendererToBrowser, DecodeError> {
  let frame = framing::decode_frame_from_bytes(data)?;
  decode_renderer_to_browser_json(frame.message_bytes).map_err(DecodeError::from)
}

/// Decode a frame from a prefix + payload byte slice and deserialize it as a
/// [`protocol::renderer::RendererToBrowser`] message.
pub fn decode_renderer_to_browser_from_parts(
  prefix: [u8; 4],
  payload: &[u8],
) -> Result<protocol::renderer::RendererToBrowser, DecodeError> {
  let frame = framing::decode_frame_from_parts(prefix, payload)?;
  decode_renderer_to_browser_json(frame.message_bytes).map_err(DecodeError::from)
}

/// Deserialize a JSON payload as a [`protocol::renderer::RendererToBrowser`] message.
pub fn decode_renderer_to_browser_json(
  json: &[u8],
) -> Result<protocol::renderer::RendererToBrowser, serde_json::Error> {
  let value: serde_json::Value = serde_json::from_slice(json)?;
  serde_json::from_value(value)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  struct SmallMsg {
    id: u32,
    text: String,
  }

  #[test]
  fn roundtrip_small_struct() {
    let msg = SmallMsg {
      id: 42,
      text: "hello".to_string(),
    };

    let mut sender = IpcConnection::new(std::io::empty(), Vec::<u8>::new());
    sender.send_json(&msg).unwrap();
    let (_, buf) = sender.into_inner();

    let mut receiver = IpcConnection::new(std::io::Cursor::new(buf), std::io::sink());
    let got: SmallMsg = receiver.recv_json().unwrap();
    assert_eq!(got, msg);
  }

  #[test]
  fn reject_sending_oversize_json_payload() {
    #[derive(serde::Serialize)]
    struct LargeMsg {
      data: String,
    }

    let msg = LargeMsg {
      // This is guaranteed to exceed the max once JSON syntax and the field name are included.
      data: "a".repeat(MAX_IPC_MESSAGE_BYTES),
    };

    let mut sender = IpcConnection::new(std::io::empty(), Vec::<u8>::new());
    let err = sender.send_json(&msg).unwrap_err();
    assert!(matches!(err, IpcError::FrameTooLarge { .. }));
  }

  #[test]
  fn reject_receiving_invalid_json() {
    // Construct a valid frame with invalid JSON payload.
    let payload = b"{not valid json}";

    let mut framed = Vec::<u8>::new();
    write_frame(&mut framed, payload).unwrap();

    let mut receiver = IpcConnection::new(std::io::Cursor::new(framed), std::io::sink());
    let err = receiver
      .recv_json::<serde_json::Value>()
      .expect_err("invalid JSON must error");

    assert!(matches!(err, IpcError::Deserialize(_)));
  }

  #[test]
  fn reject_unknown_fields_in_ipc_payloads() {
    // Regression test: IPC protocol structs must deny unknown fields so an untrusted peer cannot
    // smuggle future/ambiguous fields across the security boundary.
    //
    // Without `#[serde(deny_unknown_fields)]`, serde_json would silently ignore the `unexpected`
    // field inside `WebSocketConnectParams`.
    let payload = br#"{
      \"WebSocket\": {
        \"conn_id\": 1,
        \"cmd\": {
          \"Connect\": {
            \"params\": {
              \"url\": \"ws://example.com\",
              \"protocols\": [],
              \"origin\": null,
              \"document_url\": null,
              \"unexpected\": \"boom\"
            }
          }
        }
      }
    }"#;

    let mut framed = Vec::<u8>::new();
    write_frame(&mut framed, payload).unwrap();

    let mut receiver = IpcConnection::new(std::io::Cursor::new(framed), std::io::sink());
    let err = receiver
      .recv_json::<crate::ipc::RendererToNetwork>()
      .expect_err("unknown fields must be rejected");
    assert!(matches!(err, IpcError::Deserialize(_)));
  }

  #[test]
  fn deeply_nested_json_is_rejected_without_panicking() {
    // Threat model: the renderer process is untrusted and controls the raw IPC payload bytes. A
    // compromised/malicious renderer could send deeply-nested JSON (e.g. `[[[[...]]]]`) to try to
    // trigger a stack overflow during deserialization in the trusted browser process. We rely on
    // serde_json's built-in recursion limit as a defense-in-depth guardrail here.

    // serde_json's default recursion limit is far lower than this (currently 128); keep the input
    // small in bytes but large in nesting depth.
    //
    // Important: we embed the nested value under an *unknown* field inside a valid-looking enum
    // variant. Without the recursion guardrail, this could slip through because Serde typically
    // ignores unknown fields for forward compatibility.
    let depth: usize = 2048;
    let mut nested = String::with_capacity(depth * 2 + 1);
    for _ in 0..depth {
      nested.push('[');
    }
    nested.push('0');
    for _ in 0..depth {
      nested.push(']');
    }

    // `RendererToBrowser::HelloAck` is the smallest possible renderer → browser message. Attach the
    // malicious nesting as an unknown field inside the variant payload.
    let json = format!(r#"{{\"HelloAck\":{{\"evil\":{nested}}}}}"#);

    assert!(
      json.len() < MAX_IPC_MESSAGE_BYTES,
      "test JSON unexpectedly exceeds MAX_IPC_MESSAGE_BYTES"
    );

    let mut framed = Vec::<u8>::new();
    write_frame(&mut framed, json.as_bytes()).unwrap();

    let mut receiver = IpcConnection::new(std::io::Cursor::new(framed), std::io::sink());
    let err = receiver
      .recv_json::<protocol::renderer::RendererToBrowser>()
      .unwrap_err();

    match err {
      IpcError::Deserialize(inner) => {
        // Be robust to minor wording changes across serde_json versions.
        assert!(
          inner.to_string().to_ascii_lowercase().contains("recursion"),
          "expected recursion-limit error, got: {inner}"
        );
      }
      other => panic!("expected IpcError::Deserialize, got {other:?}"),
    }
  }
}
