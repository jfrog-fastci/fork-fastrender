use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::ipc::framing::{read_frame, write_frame};
use crate::ipc::MAX_IPC_MESSAGE_BYTES;
use crate::ipc::IpcError;
use http::{header::HeaderName, Method};
use serde::{Deserialize, Serialize};

pub type RequestId = u64;

pub const MAX_URL_BYTES: usize = 1024 * 1024;
pub const MAX_METHOD_BYTES: usize = 64;
pub const MAX_HEADER_COUNT: usize = 1024;
pub const MAX_TOTAL_HEADER_BYTES: usize = 256 * 1024;
pub const MAX_HEADER_NAME_BYTES: usize = 1024;
pub const MAX_HEADER_VALUE_BYTES: usize = 32 * 1024;
pub const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_RESPONSE_BODY_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_QUEUED_EVENTS: usize = 1024;

/// Limits controlling allocations and work for the network IPC protocol.
#[derive(Debug, Clone)]
pub struct NetworkMessageLimits {
  /// Maximum accepted byte length for URL strings.
  pub max_url_bytes: usize,
  /// Maximum accepted byte length for HTTP method strings.
  pub max_method_bytes: usize,
  /// Maximum number of headers (including duplicates).
  pub max_header_count: usize,
  /// Maximum total bytes across all header names and values.
  pub max_total_header_bytes: usize,
  /// Maximum accepted byte length of an individual header name.
  pub max_header_name_bytes: usize,
  /// Maximum accepted byte length of an individual header value.
  pub max_header_value_bytes: usize,
  /// Maximum size of a request body in bytes.
  pub max_request_body_bytes: usize,
  /// Maximum size of a response body in bytes.
  pub max_response_body_bytes: usize,
  /// Maximum size of a single event payload (e.g. websocket frame/download chunk).
  pub max_event_bytes: usize,
}

impl Default for NetworkMessageLimits {
  fn default() -> Self {
    Self {
      max_url_bytes: MAX_URL_BYTES,
      max_method_bytes: MAX_METHOD_BYTES,
      max_header_count: MAX_HEADER_COUNT,
      max_total_header_bytes: MAX_TOTAL_HEADER_BYTES,
      max_header_name_bytes: MAX_HEADER_NAME_BYTES,
      max_header_value_bytes: MAX_HEADER_VALUE_BYTES,
      max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
      max_response_body_bytes: MAX_RESPONSE_BODY_BYTES,
      // Keep event payloads comfortably under the global frame cap so routing doesn't need to
      // special-case unusually large frames.
      max_event_bytes: MAX_EVENT_BYTES,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkLimitKind {
  UrlBytes,
  MethodBytes,
  HeaderCount,
  TotalHeaderBytes,
  HeaderNameBytes,
  HeaderValueBytes,
  RequestBodyBytes,
  ResponseBodyBytes,
  EventBytes,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ValidationError {
  #[error("limit exceeded ({kind:?}): attempted {attempted} (limit {limit})")]
  LimitExceeded {
    kind: NetworkLimitKind,
    limit: usize,
    attempted: usize,
  },

  #[error("header total bytes overflowed")]
  HeaderBytesOverflow,

  #[error("invalid HTTP method")]
  InvalidMethod,

  #[error("forbidden HTTP method")]
  ForbiddenMethod,

  #[error("invalid header name")]
  InvalidHeaderName,

  #[error("invalid header value")]
  InvalidHeaderValue,

  #[error("string length exceeds u32::MAX: {len}")]
  StringLenTooLarge { len: usize },

  #[error("body length exceeds u32::MAX: {len}")]
  BodyLenTooLarge { len: usize },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DecodeError {
  #[error("unexpected end of message")]
  UnexpectedEof,

  #[error("invalid message type: {0}")]
  InvalidMessageType(u8),

  #[error("invalid event type: {0}")]
  InvalidEventType(u8),

  #[error("invalid utf-8 in {field}: {source}")]
  InvalidUtf8 {
    field: &'static str,
    #[source]
    source: std::str::Utf8Error,
  },

  #[error("invalid HTTP method")]
  InvalidMethod,

  #[error("forbidden HTTP method")]
  ForbiddenMethod,

  #[error("invalid header name")]
  InvalidHeaderName,

  #[error("invalid header value")]
  InvalidHeaderValue,

  #[error("limit exceeded ({kind:?}): attempted {attempted} (limit {limit})")]
  LimitExceeded {
    kind: NetworkLimitKind,
    limit: usize,
    attempted: usize,
  },

  #[error("trailing bytes after message: {0}")]
  TrailingBytes(usize),

  #[error("malformed message: {0}")]
  Malformed(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
  #[error("I/O error: {0}")]
  Io(#[from] std::io::Error),

  #[error("connection is closed")]
  Closed,

  #[error("duplicate request_id {request_id}")]
  DuplicateRequestId { request_id: RequestId },

  #[error("frame too large: {len} bytes (max {max})")]
  FrameTooLarge { len: usize, max: usize },

  #[error("decode error: {0}")]
  Decode(#[from] DecodeError),

  #[error("validation error: {0}")]
  Validation(#[from] ValidationError),
}

/// Errors surfaced by the network process when rejecting attacker-controlled input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum NetworkError {
  #[error("invalid request: {message}")]
  InvalidRequest { message: String },
}

impl From<TransportError> for NetworkError {
  fn from(err: TransportError) -> Self {
    Self::InvalidRequest {
      message: err.to_string(),
    }
  }
}

impl From<IpcError> for TransportError {
  fn from(err: IpcError) -> Self {
    match err {
      IpcError::Timeout => Self::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
      IpcError::Unsupported { .. } => {
        Self::Io(std::io::Error::from(std::io::ErrorKind::Unsupported))
      }
      IpcError::UnexpectedEof => Self::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
      IpcError::Io(err) => Self::Io(err),
      IpcError::FrameTooLarge { len, max } => Self::FrameTooLarge { len, max },
      IpcError::ZeroLength => Self::Decode(DecodeError::Malformed("zero-length IPC frame")),
      other => Self::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        other.to_string(),
      )),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRequest {
  pub request_id: RequestId,
  pub method: String,
  pub url: String,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkResponse {
  pub request_id: RequestId,
  pub status: u16,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkEvent {
  /// A websocket frame for an established websocket connection.
  WebSocketFrame {
    request_id: RequestId,
    is_text: bool,
    data: Vec<u8>,
  },
  /// A chunk of response body bytes for a streaming download.
  DownloadChunk {
    request_id: RequestId,
    finished: bool,
    chunk: Vec<u8>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMessage {
  Request(NetworkRequest),
  Response(NetworkResponse),
  Event(NetworkEvent),
}

impl NetworkMessage {
  pub fn request_id(&self) -> Option<RequestId> {
    match self {
      Self::Request(req) => Some(req.request_id),
      Self::Response(resp) => Some(resp.request_id),
      Self::Event(NetworkEvent::WebSocketFrame { request_id, .. })
      | Self::Event(NetworkEvent::DownloadChunk { request_id, .. }) => Some(*request_id),
    }
  }
}

fn validate_headers(
  headers: &[(String, String)],
  limits: &NetworkMessageLimits,
) -> Result<(), ValidationError> {
  if headers.len() > limits.max_header_count {
    return Err(ValidationError::LimitExceeded {
      kind: NetworkLimitKind::HeaderCount,
      limit: limits.max_header_count,
      attempted: headers.len(),
    });
  }

  let mut total: usize = 0;
  for (name, value) in headers {
    if name.as_bytes().len() > limits.max_header_name_bytes {
      return Err(ValidationError::LimitExceeded {
        kind: NetworkLimitKind::HeaderNameBytes,
        limit: limits.max_header_name_bytes,
        attempted: name.as_bytes().len(),
      });
    }
    if value.as_bytes().len() > limits.max_header_value_bytes {
      return Err(ValidationError::LimitExceeded {
        kind: NetworkLimitKind::HeaderValueBytes,
        limit: limits.max_header_value_bytes,
        attempted: value.as_bytes().len(),
      });
    }
    if HeaderName::from_bytes(name.as_bytes()).is_err() {
      return Err(ValidationError::InvalidHeaderName);
    }
    if !is_valid_header_value(value.as_bytes()) {
      return Err(ValidationError::InvalidHeaderValue);
    }

    total = total
      .checked_add(name.as_bytes().len())
      .and_then(|v| v.checked_add(value.as_bytes().len()))
      .ok_or(ValidationError::HeaderBytesOverflow)?;
    if total > limits.max_total_header_bytes {
      return Err(ValidationError::LimitExceeded {
        kind: NetworkLimitKind::TotalHeaderBytes,
        limit: limits.max_total_header_bytes,
        attempted: total,
      });
    }
  }
  Ok(())
}

fn validate_method(method: &str, limits: &NetworkMessageLimits) -> Result<(), ValidationError> {
  if method.is_empty() {
    return Err(ValidationError::InvalidMethod);
  }
  if method.as_bytes().len() > limits.max_method_bytes {
    return Err(ValidationError::LimitExceeded {
      kind: NetworkLimitKind::MethodBytes,
      limit: limits.max_method_bytes,
      attempted: method.as_bytes().len(),
    });
  }
  if Method::from_bytes(method.as_bytes()).is_err() {
    return Err(ValidationError::InvalidMethod);
  }
  if method.eq_ignore_ascii_case("CONNECT")
    || method.eq_ignore_ascii_case("TRACE")
    || method.eq_ignore_ascii_case("TRACK")
  {
    return Err(ValidationError::ForbiddenMethod);
  }
  Ok(())
}

fn is_valid_header_value(value: &[u8]) -> bool {
  // Basic hardening against header injection; keep consistent with other IPC validators.
  if value.iter().any(|&b| matches!(b, 0x00 | b'\r' | b'\n')) {
    return false;
  }
  if value.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
    return false;
  }
  if value.last().is_some_and(|b| matches!(b, b' ' | b'\t')) {
    return false;
  }
  true
}

fn encode_u16_be(out: &mut Vec<u8>, v: u16) {
  out.extend_from_slice(&v.to_be_bytes());
}

fn encode_u32_be(out: &mut Vec<u8>, v: u32) {
  out.extend_from_slice(&v.to_be_bytes());
}

fn encode_u64_be(out: &mut Vec<u8>, v: u64) {
  out.extend_from_slice(&v.to_be_bytes());
}

fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ValidationError> {
  if bytes.len() > u32::MAX as usize {
    return Err(ValidationError::BodyLenTooLarge { len: bytes.len() });
  }
  encode_u32_be(out, bytes.len() as u32);
  out.extend_from_slice(bytes);
  Ok(())
}

fn encode_string(out: &mut Vec<u8>, s: &str) -> Result<(), ValidationError> {
  let bytes = s.as_bytes();
  if bytes.len() > u32::MAX as usize {
    return Err(ValidationError::StringLenTooLarge { len: bytes.len() });
  }
  encode_u32_be(out, bytes.len() as u32);
  out.extend_from_slice(bytes);
  Ok(())
}

fn encode_message(
  msg: &NetworkMessage,
  limits: &NetworkMessageLimits,
) -> Result<Vec<u8>, ValidationError> {
  let mut out = Vec::new();
  match msg {
    NetworkMessage::Request(req) => {
      // Type tag.
      out.push(1);
      encode_u64_be(&mut out, req.request_id);

      validate_method(&req.method, limits)?;
      if req.url.as_bytes().len() > limits.max_url_bytes {
        return Err(ValidationError::LimitExceeded {
          kind: NetworkLimitKind::UrlBytes,
          limit: limits.max_url_bytes,
          attempted: req.url.as_bytes().len(),
        });
      }
      validate_headers(&req.headers, limits)?;
      if req.body.len() > limits.max_request_body_bytes {
        return Err(ValidationError::LimitExceeded {
          kind: NetworkLimitKind::RequestBodyBytes,
          limit: limits.max_request_body_bytes,
          attempted: req.body.len(),
        });
      }

      encode_string(&mut out, &req.method)?;
      encode_string(&mut out, &req.url)?;
      encode_u32_be(&mut out, req.headers.len() as u32);
      for (name, value) in &req.headers {
        encode_string(&mut out, name)?;
        encode_string(&mut out, value)?;
      }
      encode_bytes(&mut out, &req.body)?;
    }
    NetworkMessage::Response(resp) => {
      out.push(2);
      encode_u64_be(&mut out, resp.request_id);
      encode_u16_be(&mut out, resp.status);

      validate_headers(&resp.headers, limits)?;
      if resp.body.len() > limits.max_response_body_bytes {
        return Err(ValidationError::LimitExceeded {
          kind: NetworkLimitKind::ResponseBodyBytes,
          limit: limits.max_response_body_bytes,
          attempted: resp.body.len(),
        });
      }

      encode_u32_be(&mut out, resp.headers.len() as u32);
      for (name, value) in &resp.headers {
        encode_string(&mut out, name)?;
        encode_string(&mut out, value)?;
      }
      encode_bytes(&mut out, &resp.body)?;
    }
    NetworkMessage::Event(ev) => {
      out.push(3);
      match ev {
        NetworkEvent::WebSocketFrame {
          request_id,
          is_text,
          data,
        } => {
          out.push(1);
          encode_u64_be(&mut out, *request_id);
          out.push(u8::from(*is_text));
          if data.len() > limits.max_event_bytes {
            return Err(ValidationError::LimitExceeded {
              kind: NetworkLimitKind::EventBytes,
              limit: limits.max_event_bytes,
              attempted: data.len(),
            });
          }
          encode_bytes(&mut out, data)?;
        }
        NetworkEvent::DownloadChunk {
          request_id,
          finished,
          chunk,
        } => {
          out.push(2);
          encode_u64_be(&mut out, *request_id);
          out.push(u8::from(*finished));
          if chunk.len() > limits.max_event_bytes {
            return Err(ValidationError::LimitExceeded {
              kind: NetworkLimitKind::EventBytes,
              limit: limits.max_event_bytes,
              attempted: chunk.len(),
            });
          }
          encode_bytes(&mut out, chunk)?;
        }
      }
    }
  }
  Ok(out)
}

struct BytesCursor<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> BytesCursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }

  fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.pos)
  }

  fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
    let end = self
      .pos
      .checked_add(len)
      .ok_or(DecodeError::Malformed("message cursor position overflow"))?;
    if end > self.bytes.len() {
      return Err(DecodeError::UnexpectedEof);
    }
    let out = &self.bytes[self.pos..end];
    self.pos = end;
    Ok(out)
  }

  fn read_u8(&mut self) -> Result<u8, DecodeError> {
    Ok(self.take(1)?[0])
  }

  fn read_u16_be(&mut self) -> Result<u16, DecodeError> {
    let bytes = self.take(2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
  }

  fn read_u32_be(&mut self) -> Result<u32, DecodeError> {
    let bytes = self.take(4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
  }

  fn read_u64_be(&mut self) -> Result<u64, DecodeError> {
    let bytes = self.take(8)?;
    Ok(u64::from_be_bytes([
      bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
  }

  fn read_len_prefixed_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
    let len = usize::try_from(self.read_u32_be()?).map_err(|_| {
      // u32 always fits in usize on supported platforms, but keep this defensive.
      DecodeError::Malformed("length prefix does not fit in usize")
    })?;
    self.take(len)
  }
}

fn decode_headers(
  cur: &mut BytesCursor<'_>,
  limits: &NetworkMessageLimits,
) -> Result<Vec<(String, String)>, DecodeError> {
  let header_count = usize::try_from(cur.read_u32_be()?)
    .map_err(|_| DecodeError::Malformed("header count does not fit in usize"))?;
  if header_count > limits.max_header_count {
    return Err(DecodeError::LimitExceeded {
      kind: NetworkLimitKind::HeaderCount,
      limit: limits.max_header_count,
      attempted: header_count,
    });
  }

  let mut total: usize = 0;
  let mut out = Vec::with_capacity(header_count);
  for _ in 0..header_count {
    let name_bytes = cur.read_len_prefixed_bytes()?;
    let value_bytes = cur.read_len_prefixed_bytes()?;

    if name_bytes.len() > limits.max_header_name_bytes {
      return Err(DecodeError::LimitExceeded {
        kind: NetworkLimitKind::HeaderNameBytes,
        limit: limits.max_header_name_bytes,
        attempted: name_bytes.len(),
      });
    }
    if value_bytes.len() > limits.max_header_value_bytes {
      return Err(DecodeError::LimitExceeded {
        kind: NetworkLimitKind::HeaderValueBytes,
        limit: limits.max_header_value_bytes,
        attempted: value_bytes.len(),
      });
    }

    total = total
      .checked_add(name_bytes.len())
      .and_then(|v| v.checked_add(value_bytes.len()))
      .ok_or(DecodeError::Malformed("header bytes overflowed"))?;
    if total > limits.max_total_header_bytes {
      return Err(DecodeError::LimitExceeded {
        kind: NetworkLimitKind::TotalHeaderBytes,
        limit: limits.max_total_header_bytes,
        attempted: total,
      });
    }

    let name_str = std::str::from_utf8(name_bytes).map_err(|source| DecodeError::InvalidUtf8 {
      field: "header_name",
      source,
    })?;
    if HeaderName::from_bytes(name_bytes).is_err() {
      return Err(DecodeError::InvalidHeaderName);
    }

    let value_str = std::str::from_utf8(value_bytes).map_err(|source| DecodeError::InvalidUtf8 {
      field: "header_value",
      source,
    })?;
    if !is_valid_header_value(value_bytes) {
      return Err(DecodeError::InvalidHeaderValue);
    }

    out.push((name_str.to_owned(), value_str.to_owned()));
  }
  Ok(out)
}

fn decode_message(
  bytes: &[u8],
  limits: &NetworkMessageLimits,
) -> Result<NetworkMessage, DecodeError> {
  let mut cur = BytesCursor::new(bytes);
  let ty = cur.read_u8()?;
  let msg = match ty {
    1 => {
      let request_id = cur.read_u64_be()?;
      let method_bytes = cur.read_len_prefixed_bytes()?;
      if method_bytes.len() > limits.max_method_bytes {
        return Err(DecodeError::LimitExceeded {
          kind: NetworkLimitKind::MethodBytes,
          limit: limits.max_method_bytes,
          attempted: method_bytes.len(),
        });
      }
      let method_str = std::str::from_utf8(method_bytes).map_err(|source| DecodeError::InvalidUtf8 {
        field: "method",
        source,
      })?;
      if Method::from_bytes(method_bytes).is_err() {
        return Err(DecodeError::InvalidMethod);
      }
      if method_str.eq_ignore_ascii_case("CONNECT")
        || method_str.eq_ignore_ascii_case("TRACE")
        || method_str.eq_ignore_ascii_case("TRACK")
      {
        return Err(DecodeError::ForbiddenMethod);
      }
      let url_bytes = cur.read_len_prefixed_bytes()?;
      if url_bytes.len() > limits.max_url_bytes {
        return Err(DecodeError::LimitExceeded {
          kind: NetworkLimitKind::UrlBytes,
          limit: limits.max_url_bytes,
          attempted: url_bytes.len(),
        });
      }
      let url_str = std::str::from_utf8(url_bytes).map_err(|source| DecodeError::InvalidUtf8 {
        field: "url",
        source,
      })?;
      let headers = decode_headers(&mut cur, limits)?;
      let body_bytes = cur.read_len_prefixed_bytes()?;
      if body_bytes.len() > limits.max_request_body_bytes {
        return Err(DecodeError::LimitExceeded {
          kind: NetworkLimitKind::RequestBodyBytes,
          limit: limits.max_request_body_bytes,
          attempted: body_bytes.len(),
        });
      }
      let body = body_bytes.to_vec();
      NetworkMessage::Request(NetworkRequest {
        request_id,
        method: method_str.to_owned(),
        url: url_str.to_owned(),
        headers,
        body,
      })
    }
    2 => {
      let request_id = cur.read_u64_be()?;
      let status = cur.read_u16_be()?;
      let headers = decode_headers(&mut cur, limits)?;
      let body_bytes = cur.read_len_prefixed_bytes()?;
      if body_bytes.len() > limits.max_response_body_bytes {
        return Err(DecodeError::LimitExceeded {
          kind: NetworkLimitKind::ResponseBodyBytes,
          limit: limits.max_response_body_bytes,
          attempted: body_bytes.len(),
        });
      }
      let body = body_bytes.to_vec();
      NetworkMessage::Response(NetworkResponse {
        request_id,
        status,
        headers,
        body,
      })
    }
    3 => {
      let event_ty = cur.read_u8()?;
      match event_ty {
        1 => {
          let request_id = cur.read_u64_be()?;
          let is_text = match cur.read_u8()? {
            0 => false,
            1 => true,
            _ => return Err(DecodeError::Malformed("invalid is_text flag")),
          };
          let data_bytes = cur.read_len_prefixed_bytes()?;
          if data_bytes.len() > limits.max_event_bytes {
            return Err(DecodeError::LimitExceeded {
              kind: NetworkLimitKind::EventBytes,
              limit: limits.max_event_bytes,
              attempted: data_bytes.len(),
            });
          }
          if is_text {
            std::str::from_utf8(data_bytes).map_err(|source| DecodeError::InvalidUtf8 {
              field: "websocket_text_data",
              source,
            })?;
          }
          let data = data_bytes.to_vec();
          NetworkMessage::Event(NetworkEvent::WebSocketFrame {
            request_id,
            is_text,
            data,
          })
        }
        2 => {
          let request_id = cur.read_u64_be()?;
          let finished = match cur.read_u8()? {
            0 => false,
            1 => true,
            _ => return Err(DecodeError::Malformed("invalid finished flag")),
          };
          let chunk_bytes = cur.read_len_prefixed_bytes()?;
          if chunk_bytes.len() > limits.max_event_bytes {
            return Err(DecodeError::LimitExceeded {
              kind: NetworkLimitKind::EventBytes,
              limit: limits.max_event_bytes,
              attempted: chunk_bytes.len(),
            });
          }
          let chunk = chunk_bytes.to_vec();
          NetworkMessage::Event(NetworkEvent::DownloadChunk {
            request_id,
            finished,
            chunk,
          })
        }
        other => return Err(DecodeError::InvalidEventType(other)),
      }
    }
    other => return Err(DecodeError::InvalidMessageType(other)),
  };

  let trailing = cur.remaining();
  if trailing != 0 {
    return Err(DecodeError::TrailingBytes(trailing));
  }

  Ok(msg)
}

/// A framed network-protocol connection reader.
pub struct ConnectionReader<R> {
  inner: Option<R>,
  limits: NetworkMessageLimits,
}

impl<R: Read> ConnectionReader<R> {
  pub fn new(inner: R, limits: NetworkMessageLimits) -> Self {
    Self {
      inner: Some(inner),
      limits,
    }
  }

  fn close(&mut self) {
    self.inner = None;
  }

  pub fn recv(&mut self) -> Result<NetworkMessage, TransportError> {
    let Some(inner) = self.inner.as_mut() else {
      return Err(TransportError::Closed);
    };
    let payload: Vec<u8> = match read_frame(inner) {
      Ok(payload) => payload,
      Err(err) => {
        self.close();
        return Err(TransportError::from(err));
      }
    };

    match decode_message(&payload, &self.limits) {
      Ok(msg) => Ok(msg),
      Err(err) => {
        self.close();
        Err(TransportError::Decode(err))
      }
    }
  }
}

/// A framed network-protocol connection writer.
#[derive(Clone)]
pub struct ConnectionWriter<W> {
  inner: Arc<Mutex<Option<W>>>,
  limits: NetworkMessageLimits,
}

impl<W: Write> ConnectionWriter<W> {
  pub fn new(inner: W, limits: NetworkMessageLimits) -> Self {
    Self {
      inner: Arc::new(Mutex::new(Some(inner))),
      limits,
    }
  }

  fn close(&self) {
    if let Ok(mut guard) = self.inner.lock() {
      *guard = None;
    }
  }

  pub fn send(&self, msg: &NetworkMessage) -> Result<(), TransportError> {
    let payload = encode_message(msg, &self.limits)?;
    if payload.len() > MAX_IPC_MESSAGE_BYTES {
      return Err(TransportError::FrameTooLarge {
        len: payload.len(),
        max: MAX_IPC_MESSAGE_BYTES,
      });
    }

    let mut guard = self
      .inner
      .lock()
      .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "connection writer poisoned"))?;
    let Some(inner) = guard.as_mut() else {
      return Err(TransportError::Closed);
    };
    if let Err(err) = write_frame(inner, &payload) {
      self.close();
      return Err(TransportError::from(err));
    }
    if let Err(e) = inner.flush() {
      self.close();
      return Err(TransportError::Io(e));
    }
    Ok(())
  }
}

/// A bidirectional framed connection.
pub struct Connection<R, W> {
  pub reader: ConnectionReader<R>,
  pub writer: ConnectionWriter<W>,
}

impl<R: Read, W: Write> Connection<R, W> {
  pub fn new(reader: R, writer: W, limits: NetworkMessageLimits) -> Self {
    Self {
      reader: ConnectionReader::new(reader, limits.clone()),
      writer: ConnectionWriter::new(writer, limits),
    }
  }

  pub fn send(&self, msg: &NetworkMessage) -> Result<(), TransportError> {
    self.writer.send(msg)
  }

  pub fn recv(&mut self) -> Result<NetworkMessage, TransportError> {
    self.reader.recv()
  }

  pub fn split(self) -> (ConnectionReader<R>, ConnectionWriter<W>) {
    (self.reader, self.writer)
  }
}

/// A client-side helper that runs a background receiver thread and routes responses by `request_id`.
///
/// This enables request/response multiplexing and allows server→client unsolicited events to be
/// delivered without deadlocking the caller waiting on a response.
pub struct RoutedClient<W> {
  writer: ConnectionWriter<W>,
  pending: Arc<Mutex<HashMap<RequestId, mpsc::Sender<NetworkResponse>>>>,
  events_rx: mpsc::Receiver<NetworkEvent>,
  _recv_thread: thread::JoinHandle<()>,
}

impl<W: Write + Send + 'static> RoutedClient<W> {
  pub fn send_request(
    &self,
    req: NetworkRequest,
  ) -> Result<mpsc::Receiver<NetworkResponse>, TransportError> {
    let (tx, rx) = mpsc::channel();
    let request_id = req.request_id;
    {
      let mut pending = self
        .pending
        .lock()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "pending map poisoned"))?;
      if pending.contains_key(&request_id) {
        return Err(TransportError::DuplicateRequestId { request_id });
      }
      pending.insert(request_id, tx);
    }

    if let Err(e) = self.writer.send(&NetworkMessage::Request(req)) {
      let mut pending = self
        .pending
        .lock()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "pending map poisoned"))?;
      pending.remove(&request_id);
      return Err(e);
    }
    Ok(rx)
  }

  pub fn events(&self) -> &mpsc::Receiver<NetworkEvent> {
    &self.events_rx
  }

  pub fn writer(&self) -> &ConnectionWriter<W> {
    &self.writer
  }
}

pub fn spawn_routed_client<R, W>(
  reader: ConnectionReader<R>,
  writer: ConnectionWriter<W>,
) -> RoutedClient<W>
where
  R: Read + Send + 'static,
  W: Write + Send + 'static,
{
  spawn_routed_client_with_capacity(reader, writer, DEFAULT_MAX_QUEUED_EVENTS)
}

pub fn spawn_routed_client_with_capacity<R, W>(
  reader: ConnectionReader<R>,
  writer: ConnectionWriter<W>,
  max_queued_events: usize,
) -> RoutedClient<W>
where
  R: Read + Send + 'static,
  W: Write + Send + 'static,
{
  let pending: Arc<Mutex<HashMap<RequestId, mpsc::Sender<NetworkResponse>>>> =
    Arc::new(Mutex::new(HashMap::new()));
  // Use a bounded channel so a peer that floods async events cannot cause unbounded memory growth.
  let (events_tx, events_rx) = mpsc::sync_channel(max_queued_events.max(1));

  let pending_for_thread = pending.clone();
  let recv_thread = thread::spawn(move || {
    let mut reader = reader;
    loop {
      match reader.recv() {
        Ok(NetworkMessage::Response(resp)) => {
          let tx = {
            let mut pending = match pending_for_thread.lock() {
              Ok(p) => p,
              Err(poisoned) => poisoned.into_inner(),
            };
            pending.remove(&resp.request_id)
          };
          if let Some(tx) = tx {
            let _ = tx.send(resp);
          }
        }
        Ok(NetworkMessage::Event(ev)) => {
          match events_tx.try_send(ev) {
            Ok(()) => {}
            // Overflow: terminate the receiver thread so the connection is dropped and memory usage
            // stays bounded.
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => break,
          }
        }
        Ok(NetworkMessage::Request(_)) => {
          // Client should not receive requests. Treat as protocol violation and stop.
          break;
        }
        Err(_) => break,
      }
    }

    // Drop all pending response senders to unblock waiters.
    let mut pending = match pending_for_thread.lock() {
      Ok(p) => p,
      Err(poisoned) => poisoned.into_inner(),
    };
    pending.clear();
  });

  RoutedClient {
    writer,
    pending,
    events_rx,
    _recv_thread: recv_thread,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{self, Cursor};
  use std::time::Duration;

  #[derive(Clone)]
  struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

  impl Write for SharedVecWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
      let mut inner = self.0.lock().unwrap();
      inner.extend_from_slice(buf);
      Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  struct PartialRead<R> {
    inner: R,
    max_chunk: usize,
  }

  impl<R: Read> Read for PartialRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
      let max = buf.len().min(self.max_chunk);
      self.inner.read(&mut buf[..max])
    }
  }

  struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    cur: Cursor<Vec<u8>>,
  }

  impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
      Self {
        rx,
        cur: Cursor::new(Vec::new()),
      }
    }
  }

  impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
      loop {
        let n = self.cur.read(buf)?;
        if n != 0 {
          return Ok(n);
        }

        match self.rx.recv() {
          Ok(chunk) => {
            self.cur = Cursor::new(chunk);
          }
          Err(_) => return Ok(0),
        }
      }
    }
  }

  fn frame_bytes(msg: &NetworkMessage, limits: &NetworkMessageLimits) -> Vec<u8> {
    let payload = encode_message(msg, limits).unwrap();
    let mut out = Vec::new();
    write_frame(&mut out, &payload).unwrap();
    out
  }

  #[test]
  fn framing_round_trip() {
    let limits = NetworkMessageLimits::default();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = ConnectionWriter::new(SharedVecWriter(buf.clone()), limits.clone());

    let msg = NetworkMessage::Request(NetworkRequest {
      request_id: 42,
      method: "GET".to_string(),
      url: "https://example.com/".to_string(),
      headers: vec![("accept".to_string(), "*/*".to_string())],
      body: Vec::new(),
    });
    writer.send(&msg).unwrap();

    let bytes = buf.lock().unwrap().clone();
    let mut reader = ConnectionReader::new(Cursor::new(bytes), limits);
    let decoded = reader.recv().unwrap();
    assert_eq!(decoded, msg);
  }

  #[test]
  fn framing_rejects_oversized_frame() {
    let limits = NetworkMessageLimits::default();
    let len: u32 = (MAX_IPC_MESSAGE_BYTES + 1)
      .try_into()
      .expect("MAX_IPC_MESSAGE_BYTES should fit in u32 for framing");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&len.to_le_bytes());
    // No payload needed; the reader should reject based on length alone.
    let mut reader = ConnectionReader::new(Cursor::new(bytes), limits);
    let err = reader.recv().unwrap_err();
    assert!(matches!(err, TransportError::FrameTooLarge { .. }));
    // Connection is now closed.
    assert!(matches!(reader.recv().unwrap_err(), TransportError::Closed));
  }

  #[test]
  fn framing_handles_partial_reads() {
    let limits = NetworkMessageLimits::default();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = ConnectionWriter::new(SharedVecWriter(buf.clone()), limits.clone());

    let msg = NetworkMessage::Response(NetworkResponse {
      request_id: 1,
      status: 200,
      headers: vec![("content-type".to_string(), "text/plain".to_string())],
      body: b"hello".to_vec(),
    });
    writer.send(&msg).unwrap();
    let bytes = buf.lock().unwrap().clone();

    let partial = PartialRead {
      inner: Cursor::new(bytes),
      max_chunk: 1,
    };
    let mut reader = ConnectionReader::new(partial, limits);
    let decoded = reader.recv().unwrap();
    assert_eq!(decoded, msg);
  }

  fn build_request_payload(
    request_id: RequestId,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
  ) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(1);
    encode_u64_be(&mut payload, request_id);
    encode_string(&mut payload, method).unwrap();
    encode_string(&mut payload, url).unwrap();
    encode_u32_be(&mut payload, headers.len() as u32);
    for (name, value) in headers {
      encode_string(&mut payload, name).unwrap();
      encode_string(&mut payload, value).unwrap();
    }
    encode_bytes(&mut payload, body).unwrap();
    payload
  }

  fn build_ws_event_payload(request_id: RequestId, is_text: bool, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(3);
    payload.push(1);
    encode_u64_be(&mut payload, request_id);
    payload.push(u8::from(is_text));
    encode_bytes(&mut payload, data).unwrap();
    payload
  }

  fn frame_payload(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::new();
    write_frame(&mut framed, payload).unwrap();
    framed
  }

  fn recv_network_error(framed: Vec<u8>, limits: NetworkMessageLimits) -> NetworkError {
    let mut reader = ConnectionReader::new(Cursor::new(framed), limits);
    let err = reader.recv().unwrap_err();
    err.into()
  }

  #[test]
  fn rejects_oversized_url() {
    let limits = NetworkMessageLimits {
      max_url_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "aa", &[], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_invalid_method_chars() {
    let limits = NetworkMessageLimits::default();
    let payload = build_request_payload(1, "GET\n", "a", &[], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_forbidden_method() {
    let limits = NetworkMessageLimits::default();
    let payload = build_request_payload(1, "CONNECT", "a", &[], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_header_name_over_limit() {
    let limits = NetworkMessageLimits {
      max_header_name_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "a", &[("ab", "c")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_too_many_headers() {
    let limits = NetworkMessageLimits {
      max_header_count: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "a", &[("a", "b"), ("c", "d")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_total_header_bytes_over_limit() {
    let limits = NetworkMessageLimits {
      // Name+value = 1+2=3 bytes.
      max_total_header_bytes: 2,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "a", &[("a", "bc")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_header_value_over_limit() {
    let limits = NetworkMessageLimits {
      max_header_value_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "a", &[("x", "ab")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_invalid_header_name() {
    let limits = NetworkMessageLimits::default();
    let payload = build_request_payload(1, "GET", "a", &[("bad name", "x")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_invalid_header_value() {
    let limits = NetworkMessageLimits::default();
    let payload = build_request_payload(1, "GET", "a", &[("x", "bad\n")], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_body_over_limit_without_panicking() {
    let limits = NetworkMessageLimits {
      max_request_body_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "POST", "a", &[], &[1, 2]);
    let outcome = std::panic::catch_unwind(|| recv_network_error(frame_payload(&payload), limits));
    assert!(outcome.is_ok(), "expected InvalidRequest, got panic");
    assert!(matches!(
      outcome.unwrap(),
      NetworkError::InvalidRequest { .. }
    ));
  }

  #[test]
  fn rejects_method_over_limit() {
    let limits = NetworkMessageLimits {
      max_method_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_request_payload(1, "GET", "a", &[], &[]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn rejects_huge_header_count_without_panic() {
    let limits = NetworkMessageLimits {
      max_header_count: 1,
      ..NetworkMessageLimits::default()
    };
    let mut payload = Vec::new();
    payload.push(1);
    encode_u64_be(&mut payload, 1);
    encode_string(&mut payload, "GET").unwrap();
    encode_string(&mut payload, "a").unwrap();
    // Declare an absurd header count but omit any header bytes. The decoder should reject based on
    // the count alone (before allocating or attempting to read header entries).
    encode_u32_be(&mut payload, u32::MAX);

    let outcome = std::panic::catch_unwind(|| recv_network_error(frame_payload(&payload), limits));
    assert!(outcome.is_ok(), "expected InvalidRequest, got panic");
    assert!(matches!(
      outcome.unwrap(),
      NetworkError::InvalidRequest { .. }
    ));
  }

  #[test]
  fn rejects_event_over_limit() {
    let limits = NetworkMessageLimits {
      max_event_bytes: 1,
      ..NetworkMessageLimits::default()
    };
    let payload = build_ws_event_payload(1, false, &[1, 2]);
    let err = recv_network_error(frame_payload(&payload), limits);
    assert!(matches!(err, NetworkError::InvalidRequest { .. }));
  }

  #[test]
  fn queued_events_are_capped() {
    let limits = NetworkMessageLimits::default();
    let mut bytes = Vec::new();
    let ev1 = build_ws_event_payload(1, false, b"a");
    let ev2 = build_ws_event_payload(1, false, b"b");
    write_frame(&mut bytes, &ev1).unwrap();
    write_frame(&mut bytes, &ev2).unwrap();

    let reader = ConnectionReader::new(Cursor::new(bytes), limits.clone());
    let writer = ConnectionWriter::new(SharedVecWriter(Arc::new(Mutex::new(Vec::new()))), limits);
    let client = spawn_routed_client_with_capacity(reader, writer, 1);

    // Give the receiver thread time to fill the bounded channel.
    thread::sleep(Duration::from_millis(100));

    let got = client.events().recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(got, NetworkEvent::WebSocketFrame { .. }));
    let err = client
      .events()
      .recv_timeout(Duration::from_secs(1))
      .unwrap_err();
    assert!(
      matches!(err, mpsc::RecvTimeoutError::Disconnected),
      "expected overflow to terminate receiver thread, got {err:?}"
    );
  }

  #[test]
  fn malformed_messages_never_panic() {
    let limits = NetworkMessageLimits::default();
    let cases: Vec<Vec<u8>> = vec![
      vec![],
      vec![0],
      vec![1],
      vec![1, 0, 0, 0],
      {
        let mut payload = Vec::new();
        payload.push(1);
        // request_id only partially present
        payload.extend_from_slice(&[0, 0, 0]);
        payload
      },
    ];

    for payload in cases {
      let outcome = std::panic::catch_unwind(|| decode_message(&payload, &limits));
      assert!(outcome.is_ok(), "decode panicked on payload {payload:?}");
    }

  #[test]
  fn recv_closes_on_decode_error() {
    let limits = NetworkMessageLimits::default();
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &[99]).unwrap();

    let mut reader = ConnectionReader::new(Cursor::new(bytes), limits);
    let err = reader.recv().unwrap_err();
    assert!(matches!(
      err,
      TransportError::Decode(DecodeError::InvalidMessageType(99))
    ));
    assert!(matches!(reader.recv().unwrap_err(), TransportError::Closed));
  }

  #[test]
  fn recv_rejects_field_limits() {
    let writer_limits = NetworkMessageLimits::default();
    let mut reader_limits = NetworkMessageLimits::default();
    reader_limits.max_header_count = 0;

    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = ConnectionWriter::new(SharedVecWriter(buf.clone()), writer_limits);
    let msg = NetworkMessage::Response(NetworkResponse {
      request_id: 1,
      status: 200,
      headers: vec![("x".to_string(), "y".to_string())],
      body: Vec::new(),
    });
    writer.send(&msg).unwrap();

    let bytes = buf.lock().unwrap().clone();
    let mut reader = ConnectionReader::new(Cursor::new(bytes), reader_limits);
    let err = reader.recv().unwrap_err();
    assert!(matches!(
      err,
      TransportError::Decode(DecodeError::LimitExceeded {
        kind: NetworkLimitKind::HeaderCount,
        ..
      })
    ));
    assert!(matches!(reader.recv().unwrap_err(), TransportError::Closed));
  }

  #[test]
  fn routed_client_routes_responses_and_events_without_deadlock() {
    let limits = NetworkMessageLimits::default();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = ConnectionReader::new(ChannelReader::new(rx), limits.clone());

    let client_buf = Arc::new(Mutex::new(Vec::new()));
    let writer = ConnectionWriter::new(SharedVecWriter(client_buf), limits.clone());

    let client = spawn_routed_client(reader, writer);

    let response_rx = client
      .send_request(NetworkRequest {
        request_id: 7,
        method: "GET".to_string(),
        url: "https://example.com/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
      })
      .unwrap();

    // Deliver an unsolicited event first, then the response.
    tx.send(frame_bytes(
      &NetworkMessage::Event(NetworkEvent::DownloadChunk {
        request_id: 7,
        finished: true,
        chunk: b"chunk".to_vec(),
      }),
      &limits,
    ))
    .unwrap();
    tx.send(frame_bytes(
      &NetworkMessage::Response(NetworkResponse {
        request_id: 7,
        status: 200,
        headers: Vec::new(),
        body: b"ok".to_vec(),
      }),
      &limits,
    ))
    .unwrap();
    drop(tx);

    let event = client
      .events()
      .recv_timeout(Duration::from_secs(1))
      .expect("receive event");
    assert_eq!(
      event,
      NetworkEvent::DownloadChunk {
        request_id: 7,
        finished: true,
        chunk: b"chunk".to_vec(),
      }
    );

    let resp = response_rx
      .recv_timeout(Duration::from_secs(1))
      .expect("receive response");
    assert_eq!(
      resp,
      NetworkResponse {
        request_id: 7,
        status: 200,
        headers: Vec::new(),
        body: b"ok".to_vec(),
      }
    );

    // Duplicate request IDs should be rejected while the original is pending.
    let (tx2, rx2) = mpsc::channel::<Vec<u8>>();
    let reader2 = ConnectionReader::new(ChannelReader::new(rx2), limits.clone());
    let writer2 = ConnectionWriter::new(SharedVecWriter(Arc::new(Mutex::new(Vec::new()))), limits);
    let client2 = spawn_routed_client(reader2, writer2);
    let _first = client2
      .send_request(NetworkRequest {
        request_id: 1,
        method: "GET".to_string(),
        url: "https://example.com/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
      })
      .unwrap();
    let dup_err = client2
      .send_request(NetworkRequest {
        request_id: 1,
        method: "GET".to_string(),
        url: "https://example.com/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
      })
      .unwrap_err();
    assert!(matches!(
      dup_err,
      TransportError::DuplicateRequestId { request_id: 1 }
    ));
    drop(tx2);
  }
}
