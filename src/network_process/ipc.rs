use crate::error::{Error, Result};
pub use crate::net::transport::{ClientRole, NetworkError};
use crate::resource::FetchedResource;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Maximum inbound frame size (client → network process) in bytes.
pub const MAX_INBOUND_FRAME_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Maximum outbound frame size (network process → client) in bytes.
///
/// Responses contain base64-encoded bodies. The in-process `HttpFetcher` defaults to a 50 MiB body
/// cap, which expands to ~67 MiB after base64, so we keep a margin.
pub const MAX_OUTBOUND_FRAME_BYTES: usize = 80 * 1024 * 1024; // 80 MiB

/// Maximum decoded response body size (bytes).
///
/// Keep this aligned with the default `ResourcePolicy::max_response_bytes` so a compromised network
/// process cannot force unbounded allocations in the browser.
const MAX_RESPONSE_BODY_BYTES: usize = 50 * 1024 * 1024; // 50 MiB

/// Maximum accepted URL string length (bytes).
pub const MAX_URL_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum accepted auth token length (bytes).
pub const MAX_AUTH_TOKEN_BYTES: usize = 1024;

fn closed_connection_err() -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::NotConnected, "network IPC connection is closed")
}

/// Length-prefixed JSON connection used by the browser-side network client.
///
/// On any protocol violation (oversized frame, truncated frame, JSON decode failure), the
/// connection is permanently marked closed and subsequent reads/writes error deterministically.
#[derive(Debug)]
pub struct NetworkClient<S> {
  stream: S,
  closed: bool,
}

impl<S> NetworkClient<S> {
  pub fn new(stream: S) -> Self {
    Self { stream, closed: false }
  }

  pub fn is_closed(&self) -> bool {
    self.closed
  }

  pub fn into_inner(self) -> S {
    self.stream
  }

  fn mark_closed(&mut self) {
    self.closed = true;
  }
}

impl<S: Read + Write> NetworkClient<S> {
  pub fn send_request<T: Serialize>(&mut self, msg: &T) -> std::io::Result<()> {
    if self.closed {
      return Err(closed_connection_err());
    }
    let res = write_request_frame(&mut self.stream, msg);
    if res.is_err() {
      self.mark_closed();
    }
    res
  }

  pub fn recv_response<T: DeserializeOwned>(&mut self) -> std::io::Result<T> {
    if self.closed {
      return Err(closed_connection_err());
    }
    let res = read_response_frame(&mut self.stream);
    if res.is_err() {
      self.mark_closed();
    }
    res
  }
}

/// Length-prefixed JSON connection used by the network process.
///
/// On any protocol violation (oversized frame, truncated frame, JSON decode failure), the
/// connection is permanently marked closed and subsequent reads/writes error deterministically.
#[derive(Debug)]
pub struct NetworkService<S> {
  stream: S,
  closed: bool,
}

impl<S> NetworkService<S> {
  pub fn new(stream: S) -> Self {
    Self { stream, closed: false }
  }

  pub fn is_closed(&self) -> bool {
    self.closed
  }

  pub fn into_inner(self) -> S {
    self.stream
  }

  fn mark_closed(&mut self) {
    self.closed = true;
  }
}

impl<S: Read + Write> NetworkService<S> {
  pub fn recv_request<T: DeserializeOwned>(&mut self) -> std::io::Result<T> {
    if self.closed {
      return Err(closed_connection_err());
    }
    let res = read_request_frame(&mut self.stream);
    if res.is_err() {
      self.mark_closed();
    }
    res
  }

  pub fn send_response<T: Serialize>(&mut self, msg: &T) -> std::io::Result<()> {
    if self.closed {
      return Err(closed_connection_err());
    }
    let res = write_response_frame(&mut self.stream, msg);
    if res.is_err() {
      self.mark_closed();
    }
    res
  }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum NetworkRequest {
  Hello { token: String, role: ClientRole },
  Fetch { url: String },
  /// Begin a streaming download (browser-only).
  ///
  /// After acknowledging with [`NetworkResponse::DownloadStarted`], the network process may emit one
  /// or more [`NetworkResponse::DownloadChunk`] messages before closing the connection.
  DownloadStart { url: String },
  Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum NetworkResponse {
  HelloAck,
  FetchOk { resource: IpcFetchedResource },
  DownloadStarted {
    download_id: u64,
    total_bytes: Option<u64>,
  },
  DownloadChunk {
    download_id: u64,
    finished: bool,
    bytes_base64: String,
  },
  Ok,
  Error { error: NetworkError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcFetchedResource {
  pub bytes_base64: String,
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub response_headers: Option<Vec<(String, String)>>,
}

impl IpcFetchedResource {
  pub fn from_fetched(resource: FetchedResource) -> Self {
    Self {
      bytes_base64: BASE64_STANDARD.encode(&resource.bytes),
      content_type: resource.content_type,
      nosniff: resource.nosniff,
      content_encoding: resource.content_encoding,
      status: resource.status,
      etag: resource.etag,
      last_modified: resource.last_modified,
      access_control_allow_origin: resource.access_control_allow_origin,
      timing_allow_origin: resource.timing_allow_origin,
      vary: resource.vary,
      access_control_allow_credentials: resource.access_control_allow_credentials,
      final_url: resource.final_url,
      response_headers: resource.response_headers,
    }
  }

  pub fn into_fetched(self) -> Result<FetchedResource> {
    let upper = base64_decoded_len_upper_bound(&self.bytes_base64).ok_or_else(|| {
      Error::Other("invalid base64 bytes from network process: invalid length".to_string())
    })?;
    if upper > MAX_RESPONSE_BODY_BYTES {
      return Err(Error::Other(format!(
        "invalid base64 bytes from network process: decoded length upper bound {upper} exceeds hard limit {MAX_RESPONSE_BODY_BYTES}"
      )));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(upper).map_err(|err| {
      Error::Other(format!(
        "invalid base64 bytes from network process: allocation failed (len={upper}): {err:?}"
      ))
    })?;
    BASE64_STANDARD
      .decode_vec(self.bytes_base64.as_bytes(), &mut bytes)
      .map_err(|err| Error::Other(format!("invalid base64 bytes from network process: {err}")))?;
    let mut res = FetchedResource::new(bytes, self.content_type);
    res.nosniff = self.nosniff;
    res.content_encoding = self.content_encoding;
    res.status = self.status;
    res.etag = self.etag;
    res.last_modified = self.last_modified;
    res.access_control_allow_origin = self.access_control_allow_origin;
    res.timing_allow_origin = self.timing_allow_origin;
    res.vary = self.vary;
    res.access_control_allow_credentials = self.access_control_allow_credentials;
    res.final_url = self.final_url;
    res.response_headers = self.response_headers;
    Ok(res)
  }
}

fn serde_err_to_io(err: serde_json::Error) -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

fn base64_decoded_len_upper_bound(encoded: &str) -> Option<usize> {
  // We use the standard padded base64 alphabet. The output length is:
  //
  //   decoded = (len / 4) * 3 - padding
  //
  // where padding is 0, 1, or 2 depending on the number of trailing '='.
  let encoded_len = encoded.len();
  if encoded_len % 4 != 0 {
    return None;
  }
  let groups = encoded_len.checked_div(4)?;
  let mut decoded = groups.checked_mul(3)?;

  if encoded.ends_with("==") {
    decoded = decoded.checked_sub(2)?;
  } else if encoded.ends_with('=') {
    decoded = decoded.checked_sub(1)?;
  }

  Some(decoded)
}

fn try_alloc_frame_buf(len: usize) -> std::io::Result<Vec<u8>> {
  let mut buf = Vec::new();
  buf.try_reserve_exact(len).map_err(|err| {
    std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("IPC frame allocation failed (len={len}): {err:?}"),
    )
  })?;
  buf.resize(len, 0);
  Ok(buf)
}

fn validate_frame_len(declared_len: u32, max_frame_bytes: usize) -> std::io::Result<usize> {
  if declared_len == 0 {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidData,
      "zero-length IPC frame",
    ));
  }
  let max_u32: u32 = max_frame_bytes.try_into().map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "max_frame_bytes exceeds u32::MAX")
  })?;
  if declared_len > max_u32 {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidData,
      format!("IPC frame too large: {declared_len} bytes (max {max_frame_bytes})"),
    ));
  }
  usize::try_from(declared_len).map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame length does not fit usize")
  })
}

/// Write a length-prefixed JSON frame, enforcing `max_frame_bytes` before sending.
pub fn write_frame_with_limit<W: Write, T: Serialize>(
  writer: &mut W,
  msg: &T,
  max_frame_bytes: usize,
) -> std::io::Result<()> {
  let bytes = serde_json::to_vec(msg).map_err(serde_err_to_io)?;
  if bytes.is_empty() {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidInput,
      "cannot write zero-length IPC frame",
    ));
  }
  if bytes.len() > max_frame_bytes {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidInput,
      format!("IPC frame too large: {} bytes (max {max_frame_bytes})", bytes.len()),
    ));
  }
  let len: u32 = bytes.len().try_into().map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame length exceeds u32::MAX")
  })?;
  writer.write_all(&len.to_be_bytes())?;
  writer.write_all(&bytes)?;
  writer.flush()?;
  Ok(())
}

/// Read a length-prefixed JSON frame, enforcing `max_frame_bytes` before allocating.
pub fn read_frame_with_limit<R: Read, T: DeserializeOwned>(
  reader: &mut R,
  max_frame_bytes: usize,
) -> std::io::Result<T> {
  let mut len_buf = [0u8; 4];
  reader.read_exact(&mut len_buf)?;
  let declared_len = u32::from_be_bytes(len_buf);
  let len = validate_frame_len(declared_len, max_frame_bytes)?;
  let mut buf = try_alloc_frame_buf(len)?;
  reader.read_exact(&mut buf)?;
  serde_json::from_slice(&buf).map_err(serde_err_to_io)
}

/// Write a client→network request frame.
pub fn write_request_frame<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> std::io::Result<()> {
  write_frame_with_limit(writer, msg, MAX_INBOUND_FRAME_BYTES)
}

/// Write a network→client response frame.
pub fn write_response_frame<W: Write, T: Serialize>(
  writer: &mut W,
  msg: &T,
) -> std::io::Result<()> {
  write_frame_with_limit(writer, msg, MAX_OUTBOUND_FRAME_BYTES)
}

/// Read a client→network request frame.
pub fn read_request_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> std::io::Result<T> {
  read_frame_with_limit(reader, MAX_INBOUND_FRAME_BYTES)
}

/// Read a network→client response frame.
pub fn read_response_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> std::io::Result<T> {
  read_frame_with_limit(reader, MAX_OUTBOUND_FRAME_BYTES)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
  struct Dummy {
    n: u32,
  }

  fn frame_bytes(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).expect("payload length should fit u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
  }

  #[test]
  fn oversized_frame_length_is_a_fatal_protocol_error() {
    let valid = frame_bytes(br#"{"n":1}"#);

    let oversized: u32 = (MAX_INBOUND_FRAME_BYTES + 1)
      .try_into()
      .expect("MAX_INBOUND_FRAME_BYTES should fit u32 for framing");
    let mut buf = Vec::new();
    buf.extend_from_slice(&oversized.to_be_bytes());
    buf.extend_from_slice(&valid);

    let cursor = Cursor::new(buf);
    let mut conn = NetworkService::new(cursor);

    let err = conn
      .recv_request::<Dummy>()
      .expect_err("oversized frame should error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // Even though a valid frame follows, the connection must be poisoned/closed.
    let err2 = conn
      .recv_request::<Dummy>()
      .expect_err("protocol violation should close connection");
    assert_eq!(err2.kind(), std::io::ErrorKind::NotConnected);
  }

  #[test]
  fn truncated_frame_is_a_fatal_protocol_error() {
    // Declares 10 bytes, only provides 5.
    let mut buf = Vec::new();
    buf.extend_from_slice(&10u32.to_be_bytes());
    buf.extend_from_slice(b"hello");

    let cursor = Cursor::new(buf);
    let mut conn = NetworkService::new(cursor);

    let err = conn
      .recv_request::<Dummy>()
      .expect_err("truncated frame should error");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let err2 = conn
      .recv_request::<Dummy>()
      .expect_err("I/O failure should close connection");
    assert_eq!(err2.kind(), std::io::ErrorKind::NotConnected);
  }

  #[test]
  fn invalid_json_payload_is_a_fatal_protocol_error() {
    let valid = frame_bytes(br#"{"n":2}"#);

    let junk = [0xFFu8, 0xFEu8, 0xFFu8];
    let mut buf = Vec::new();
    buf.extend_from_slice(&(junk.len() as u32).to_be_bytes());
    buf.extend_from_slice(&junk);
    buf.extend_from_slice(&valid);

    let cursor = Cursor::new(buf);
    let mut conn = NetworkClient::new(cursor);

    let err = conn
      .recv_response::<Dummy>()
      .expect_err("invalid JSON should error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // Even though a valid frame follows, the connection must be poisoned/closed.
    let err2 = conn
      .recv_response::<Dummy>()
      .expect_err("JSON decode failure should close connection");
    assert_eq!(err2.kind(), std::io::ErrorKind::NotConnected);
  }
}
