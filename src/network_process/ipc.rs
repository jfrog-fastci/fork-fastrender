use crate::error::{Error, Result};
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

/// Maximum accepted URL string length (bytes).
pub const MAX_URL_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum accepted auth token length (bytes).
pub const MAX_AUTH_TOKEN_BYTES: usize = 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetworkRequest {
  Hello { token: String },
  Fetch { url: String },
  Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetworkResponse {
  HelloAck,
  FetchOk { resource: IpcFetchedResource },
  Ok,
  Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let bytes = BASE64_STANDARD
      .decode(self.bytes_base64.as_bytes())
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
  let mut buf = vec![0u8; len];
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
