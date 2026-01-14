use super::{
  web_fetch, CacheArtifactKind, DocumentOrigin, FetchContextKind, FetchCredentialsMode,
  FetchDestination, FetchRequest, FetchedResource, HttpCachePolicy, HttpRequest, ParsedContentRange,
  ReferrerPolicy, ResourceFetcher,
};
use super::parse_content_range;
use crate::error::{Error, ResourceError, Result};
use base64::Engine as _;
use http::header::HeaderName;
use http::Method;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::UNIX_EPOCH;
use url::Url;

const IPC_FRAME_LEN_BYTES: usize = 4;
const MAX_COOKIE_BYTES: usize = 4096;
// Avoid deadlocking the renderer if the network process becomes unresponsive mid-request.
const IPC_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Environment variable used by [`IpcResourceFetcher::new`] to read the IPC auth token.
pub const IPC_AUTH_TOKEN_ENV: &str = "FASTR_NETWORK_AUTH_TOKEN";

/// Maximum inbound frame size (renderer → network process) in bytes.
///
/// This is a security limit: the network process must reject any request frame exceeding this size
/// **before** allocating.
pub const IPC_MAX_INBOUND_FRAME_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Maximum outbound frame size (network process → renderer) in bytes.
///
/// Responses include base64-encoded bodies, so this limit must exceed the underlying fetcher's
/// `max_response_bytes` budget. The default `ResourcePolicy` cap is 50 MiB, which expands to ~67 MiB
/// when base64-encoded; keep a comfortable margin.
pub const IPC_MAX_OUTBOUND_FRAME_BYTES: usize = 80 * 1024 * 1024; // 80 MiB

/// Maximum URL string length accepted by the network process (in bytes).
///
/// Note: this limit is intended for *network* URLs (http/https). Other URL schemes (notably `data:`)
/// can legitimately be larger; those use [`IPC_MAX_NON_HTTP_URL_BYTES`].
pub const IPC_MAX_URL_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum number of request headers accepted by the network process.
pub const IPC_MAX_HEADER_COUNT: usize = 256;

/// Maximum byte length for a single request header name.
pub const IPC_MAX_HEADER_NAME_BYTES: usize = 1024;

/// Maximum byte length for a single request header value.
pub const IPC_MAX_HEADER_VALUE_BYTES: usize = 1024;

/// Maximum URL length (in bytes) accepted for non-http(s) schemes (e.g. data/file/about).
///
/// This is still bounded to avoid pathological IPC payloads, but is intentionally larger than
/// [`IPC_MAX_URL_BYTES`] so inline resources like `data:` URLs do not get truncated prematurely.
pub const IPC_MAX_NON_HTTP_URL_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum decoded request body size (in bytes) accepted in [`IpcHttpRequest::body_b64`].
///
/// Larger uploads should be implemented using chunked/streaming request bodies instead of sending a
/// single base64 blob over IPC.
pub const IPC_MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum byte length for the HTTP method string in [`IpcHttpRequest`].
pub const IPC_MAX_METHOD_BYTES: usize = 64;

/// Maximum byte length for the auth token string.
pub const IPC_MAX_AUTH_TOKEN_BYTES: usize = 1024;

/// Maximum response body size accepted by the IPC client when reassembling chunked payloads.
///
/// Keep this aligned with `ResourcePolicy::max_response_bytes` (50 MiB by default) so the network
/// process cannot force unbounded allocations even if it misbehaves.
const IPC_MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Inline response body limit for a single IPC response.
///
/// When the response body exceeds this size, the network process must send it using chunked
/// transfer messages so that no single IPC response frame needs to contain the entire body.
const IPC_INLINE_LIMIT_BYTES: usize = 1 * 1024 * 1024;

/// Maximum raw bytes per body chunk message.
///
/// This is intentionally small so that even JSON + base64 encoding stays well under the global IPC
/// frame caps.
const IPC_CHUNK_MAX_BYTES: usize = 64 * 1024;

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn is_http_or_https_url(url: &str) -> bool {
  starts_with_ignore_ascii_case(url, "http://") || starts_with_ignore_ascii_case(url, "https://")
}

fn contains_nul_cr_or_lf(s: &str) -> bool {
  s.as_bytes()
    .iter()
    .any(|&b| b == 0x00 || b == b'\r' || b == b'\n')
}

fn contains_nul(s: &str) -> bool {
  s.as_bytes().contains(&0x00)
}

fn write_ipc_frame<W: Write>(
  writer: &mut W,
  payload: &[u8],
  max_frame_bytes: usize,
) -> io::Result<()> {
  if payload.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "IPC frame payload cannot be empty",
    ));
  }
  if payload.len() > max_frame_bytes {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "IPC frame too large: {} bytes (max {max_frame_bytes})",
        payload.len()
      ),
    ));
  }
  if payload.len() > u32::MAX as usize {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "IPC frame too large",
    ));
  }
  let len = (payload.len() as u32).to_le_bytes();
  writer.write_all(&len)?;
  writer.write_all(payload)?;
  writer.flush()?;
  Ok(())
}

fn read_ipc_frame<R: Read>(reader: &mut R, max_frame_bytes: usize) -> io::Result<Vec<u8>> {
  let mut len_buf = [0u8; IPC_FRAME_LEN_BYTES];
  reader.read_exact(&mut len_buf)?;
  let len = u32::from_le_bytes(len_buf) as usize;
  if len == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "IPC frame declared length is zero",
    ));
  }
  if len > max_frame_bytes {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame too large: {len} bytes (max {max_frame_bytes})"),
    ));
  }
  let mut buf = Vec::new();
  buf.try_reserve_exact(len).map_err(|err| {
    io::Error::new(
      io::ErrorKind::Other,
      format!("IPC frame allocation failed (len={len}): {err:?}"),
    )
  })?;
  buf.resize(len, 0);
  reader.read_exact(&mut buf)?;
  Ok(buf)
}

/// Envelope used to correlate multi-frame responses with a request.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserToNetwork {
  pub id: u64,
  pub request: IpcRequest,
}

/// Fetch metadata transferred ahead of a chunked body stream.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcFetchedResourceMeta {
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub response_referrer_policy: Option<ReferrerPolicy>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<IpcHttpCachePolicy>,
  pub response_headers: Option<Vec<(String, String)>>,
}

impl IpcFetchedResourceMeta {
  fn from_fetched_resource(res: &FetchedResource) -> Self {
    Self {
      content_type: res.content_type.clone(),
      nosniff: res.nosniff,
      content_encoding: res.content_encoding.clone(),
      status: res.status,
      etag: res.etag.clone(),
      last_modified: res.last_modified.clone(),
      access_control_allow_origin: res.access_control_allow_origin.clone(),
      timing_allow_origin: res.timing_allow_origin.clone(),
      vary: res.vary.clone(),
      response_referrer_policy: res.response_referrer_policy,
      access_control_allow_credentials: res.access_control_allow_credentials,
      final_url: res.final_url.clone(),
      cache_policy: res.cache_policy.as_ref().map(IpcHttpCachePolicy::from),
      response_headers: res.response_headers.clone(),
    }
  }

  fn into_fetched_resource(self, bytes: Vec<u8>) -> FetchedResource {
    FetchedResource {
      bytes,
      content_type: self.content_type,
      nosniff: self.nosniff,
      content_encoding: self.content_encoding,
      status: self.status,
      etag: self.etag,
      last_modified: self.last_modified,
      access_control_allow_origin: self.access_control_allow_origin,
      timing_allow_origin: self.timing_allow_origin,
      vary: self.vary,
      response_referrer_policy: self.response_referrer_policy,
      access_control_allow_credentials: self.access_control_allow_credentials,
      final_url: self.final_url,
      cache_policy: self.cache_policy.map(Into::into),
      response_headers: self.response_headers,
    }
  }
}

/// Messages sent from the network process back to the browser/renderer over the IPC channel.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkToBrowser {
  /// Single-frame RPC response (used for non-fetch RPCs and small bodies).
  Response { id: u64, response: IpcResponse },

  /// Start of a chunked fetch response body stream.
  FetchStart {
    id: u64,
    meta: IpcFetchedResourceMeta,
    total_len: usize,
  },

  /// A chunk of base64-encoded response body bytes.
  ///
  /// `bytes_b64` decodes to at most `IPC_CHUNK_MAX_BYTES` bytes.
  FetchBodyChunk { id: u64, bytes_b64: String },

  /// End of a chunked fetch response body stream.
  FetchEnd { id: u64 },

  /// Chunked fetch error response.
  FetchErr { id: u64, err: IpcError },
}

#[derive(Debug)]
enum RpcReply {
  Response(IpcResponse),
  ChunkedFetch(IpcResult<(IpcFetchedResourceMeta, Vec<u8>)>),
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

/// Server-side helper for writing responses on the browser↔network IPC channel.
///
/// This implements chunked body transfer for large responses so that no individual IPC frame has to
/// contain an entire response body.
#[doc(hidden)]
pub struct NetworkService<'a, W: Write> {
  writer: &'a mut W,
}

impl<'a, W: Write> NetworkService<'a, W> {
  pub fn new(writer: &'a mut W) -> Self {
    Self { writer }
  }

  pub fn send_message(&mut self, msg: &NetworkToBrowser) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(|err| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("failed to serialize IPC message: {err}"),
      )
    })?;
    write_ipc_frame(self.writer, &payload, IPC_MAX_OUTBOUND_FRAME_BYTES)
  }

  pub fn send_response(&mut self, id: u64, response: IpcResponse) -> io::Result<()> {
    self.send_message(&NetworkToBrowser::Response { id, response })
  }

  pub fn send_fetch_result(&mut self, id: u64, result: Result<FetchedResource>) -> io::Result<()> {
    match result {
      Ok(res) => self.send_fetch_ok(id, res),
      Err(err) => {
        let err = IpcError::from(err);
        self.send_message(&NetworkToBrowser::FetchErr { id, err })
      }
    }
  }

  pub fn send_fetch_ok(&mut self, id: u64, res: FetchedResource) -> io::Result<()> {
    if res.bytes.len() <= IPC_INLINE_LIMIT_BYTES {
      // Legacy single-frame response.
      let response = IpcResponse::Fetched(IpcResult::Ok(res.into()));
      return self.send_response(id, response);
    }

    let total_len = res.bytes.len();
    let meta = IpcFetchedResourceMeta::from_fetched_resource(&res);
    self.send_message(&NetworkToBrowser::FetchStart {
      id,
      meta,
      total_len,
    })?;

    for chunk in res.bytes.chunks(IPC_CHUNK_MAX_BYTES) {
      let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
      self.send_message(&NetworkToBrowser::FetchBodyChunk { id, bytes_b64 })?;
    }

    self.send_message(&NetworkToBrowser::FetchEnd { id })
  }
}

fn validate_ipc_url(url: &str) -> std::result::Result<(), String> {
  let is_http_like = is_http_or_https_url(url);
  let max_len = if is_http_like {
    IPC_MAX_URL_BYTES
  } else {
    IPC_MAX_NON_HTTP_URL_BYTES
  };
  if url.len() > max_len {
    return Err(format!("url exceeds max length ({} > {})", url.len(), max_len));
  }
  // NUL bytes are always rejected.
  if contains_nul(url) {
    return Err("url contains NUL byte".to_string());
  }
  // CR/LF bytes are rejected for network URLs (avoid header injection and parsing ambiguity).
  if is_http_like && url.as_bytes().iter().any(|&b| b == b'\r' || b == b'\n') {
    return Err("url contains CR/LF bytes".to_string());
  }
  Ok(())
}

fn validate_ipc_headers(headers: &[(String, String)]) -> std::result::Result<(), String> {
  if headers.len() > IPC_MAX_HEADER_COUNT {
    return Err(format!(
      "header count exceeds max ({} > {})",
      headers.len(),
      IPC_MAX_HEADER_COUNT
    ));
  }
  for (name, value) in headers {
    if name.is_empty() {
      return Err("header name is empty".to_string());
    }
    if name.len() > IPC_MAX_HEADER_NAME_BYTES {
      return Err(format!(
        "header name exceeds max length ({} > {})",
        name.len(),
        IPC_MAX_HEADER_NAME_BYTES
      ));
    }
    if value.len() > IPC_MAX_HEADER_VALUE_BYTES {
      return Err(format!(
        "header value exceeds max length ({} > {})",
        value.len(),
        IPC_MAX_HEADER_VALUE_BYTES
      ));
    }

    if contains_nul_cr_or_lf(name) {
      return Err("header name contains NUL/CR/LF bytes".to_string());
    }
    if contains_nul_cr_or_lf(value) {
      return Err("header value contains NUL/CR/LF bytes".to_string());
    }
    if HeaderName::from_bytes(name.as_bytes()).is_err() {
      return Err("header name is not a valid HTTP token".to_string());
    }
  }
  Ok(())
}

fn validate_ipc_method(method: &str) -> std::result::Result<(), String> {
  if method.is_empty() {
    return Err("method is empty".to_string());
  }
  if method.len() > IPC_MAX_METHOD_BYTES {
    return Err(format!(
      "method exceeds max length ({} > {})",
      method.len(),
      IPC_MAX_METHOD_BYTES
    ));
  }
  if contains_nul_cr_or_lf(method) {
    return Err("method contains NUL/CR/LF bytes".to_string());
  }
  if Method::from_bytes(method.as_bytes()).is_err() {
    return Err("method is not a valid HTTP token".to_string());
  }
  Ok(())
}

fn estimated_base64_decoded_len(encoded_len: usize) -> Option<usize> {
  // Conservative upper bound for base64 decoded bytes, accounting for unpadded encodings:
  // - Each 4 chars yields up to 3 bytes.
  // - Remainders yield up to 0/1/2 bytes (1 is invalid base64 and will error on decode anyway).
  let chunks = encoded_len.checked_div(4)?;
  let rem = encoded_len % 4;
  let mut out = chunks.checked_mul(3)?;
  let extra = match rem {
    0 => 0,
    1 => 0,
    2 => 1,
    3 => 2,
    _ => 0,
  };
  out = out.checked_add(extra)?;
  Some(out)
}

fn validate_ipc_body(body_b64: &str) -> std::result::Result<(), String> {
  let Some(estimated) = estimated_base64_decoded_len(body_b64.len()) else {
    return Err("request body length overflow".to_string());
  };
  if estimated > IPC_MAX_REQUEST_BODY_BYTES {
    return Err(format!(
      "request body exceeds max length (estimated {} > {})",
      estimated, IPC_MAX_REQUEST_BODY_BYTES
    ));
  }
  Ok(())
}

fn validate_ipc_fetch_request(req: &IpcFetchRequest) -> std::result::Result<(), String> {
  validate_ipc_url(&req.url)?;
  if let Some(referrer) = &req.referrer_url {
    validate_ipc_url(referrer)?;
  }
  Ok(())
}

/// Validate an incoming [`IpcRequest`] against hard protocol limits.
///
/// The network process must treat any violation as a protocol error and close the connection.
pub fn validate_ipc_request(request: &IpcRequest) -> std::result::Result<(), String> {
  match request {
    IpcRequest::Hello { token } => {
      if token.is_empty() {
        return Err("auth token is empty".to_string());
      }
      if contains_nul_cr_or_lf(token) {
        return Err("auth token contains NUL/CR/LF bytes".to_string());
      }
      if token.len() > IPC_MAX_AUTH_TOKEN_BYTES {
        return Err(format!(
          "auth token exceeds max length ({} > {})",
          token.len(),
          IPC_MAX_AUTH_TOKEN_BYTES
        ));
      }
      Ok(())
    }
    IpcRequest::Fetch { url }
    | IpcRequest::CookieHeaderValue { url }
    | IpcRequest::FetchPartialWithContext { url, .. }
    | IpcRequest::ReadCacheArtifact { url, .. }
    | IpcRequest::WriteCacheArtifact { url, .. }
    | IpcRequest::RemoveCacheArtifact { url, .. } => validate_ipc_url(url),
    IpcRequest::StoreCookieFromDocument { url, cookie_string } => {
      validate_ipc_url(url)?;
      if cookie_string.len() > MAX_COOKIE_BYTES {
        return Err(format!(
          "cookie_string exceeds max length ({} > {})",
          cookie_string.len(),
          MAX_COOKIE_BYTES
        ));
      }
      Ok(())
    }
    IpcRequest::FetchWithRequest { req }
    | IpcRequest::FetchWithRequestAndValidation { req, .. }
    | IpcRequest::FetchPartialWithRequest { req, .. }
    | IpcRequest::ReadCacheArtifactWithRequest { req, .. }
    | IpcRequest::WriteCacheArtifactWithRequest { req, .. }
    | IpcRequest::RemoveCacheArtifactWithRequest { req, .. } => validate_ipc_fetch_request(req),
    IpcRequest::FetchHttpRequest { req } => {
      validate_ipc_fetch_request(&req.fetch)?;
      validate_ipc_method(&req.method)?;
      validate_ipc_headers(&req.headers)?;
      if let Some(body_b64) = req.body_b64.as_deref() {
        validate_ipc_body(body_b64)?;
      }
      Ok(())
    }
    IpcRequest::RequestHeaderValue { req, header_name } => {
      validate_ipc_fetch_request(req)?;
      if header_name.len() > IPC_MAX_HEADER_NAME_BYTES {
        return Err(format!(
          "header_name exceeds max length ({} > {})",
          header_name.len(),
          IPC_MAX_HEADER_NAME_BYTES
        ));
      }
      if contains_nul_cr_or_lf(header_name) {
        return Err("header_name contains NUL/CR/LF bytes".to_string());
      }
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validate_ipc_request_rejects_overlong_http_url() {
    let base = "https://example.com/";
    let pad = "a".repeat(IPC_MAX_URL_BYTES - base.len() + 1);
    let url = format!("{base}{pad}");
    assert!(url.len() > IPC_MAX_URL_BYTES);
    let err = validate_ipc_request(&IpcRequest::Fetch { url }).unwrap_err();
    assert!(err.contains("url exceeds max length"), "unexpected error: {err}");
  }

  #[test]
  fn validate_ipc_request_rejects_too_many_headers() {
    let mut headers = Vec::new();
    for idx in 0..=IPC_MAX_HEADER_COUNT {
      headers.push((format!("x-test-{idx}"), "a".to_string()));
    }
    let req = IpcRequest::FetchHttpRequest {
      req: IpcHttpRequest {
        fetch: IpcFetchRequest {
          url: "https://example.com/".to_string(),
          destination: FetchDestination::Fetch,
          referrer_url: None,
          client_origin: None,
          referrer_policy: ReferrerPolicy::NoReferrer,
          credentials_mode: FetchCredentialsMode::Omit,
        },
        method: "GET".to_string(),
        redirect: web_fetch::RequestRedirect::Follow,
        headers,
        body_b64: None,
      },
    };
    let err = validate_ipc_request(&req).unwrap_err();
    assert!(err.contains("header count exceeds max"), "unexpected error: {err}");
  }

  #[test]
  fn validate_ipc_request_rejects_overlong_header_value() {
    let value = "a".repeat(IPC_MAX_HEADER_VALUE_BYTES + 1);
    let req = IpcRequest::FetchHttpRequest {
      req: IpcHttpRequest {
        fetch: IpcFetchRequest {
          url: "https://example.com/".to_string(),
          destination: FetchDestination::Fetch,
          referrer_url: None,
          client_origin: None,
          referrer_policy: ReferrerPolicy::NoReferrer,
          credentials_mode: FetchCredentialsMode::Omit,
        },
        method: "GET".to_string(),
        redirect: web_fetch::RequestRedirect::Follow,
        headers: vec![("X-Test".to_string(), value)],
        body_b64: None,
      },
    };
    let err = validate_ipc_request(&req).unwrap_err();
    assert!(
      err.contains("header value exceeds max length"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn validate_ipc_request_rejects_header_value_with_newlines() {
    let req = IpcRequest::FetchHttpRequest {
      req: IpcHttpRequest {
        fetch: IpcFetchRequest {
          url: "https://example.com/".to_string(),
          destination: FetchDestination::Fetch,
          referrer_url: None,
          client_origin: None,
          referrer_policy: ReferrerPolicy::NoReferrer,
          credentials_mode: FetchCredentialsMode::Omit,
        },
        method: "GET".to_string(),
        redirect: web_fetch::RequestRedirect::Follow,
        headers: vec![("X-Test".to_string(), "hello\r\nworld".to_string())],
        body_b64: None,
      },
    };
    let err = validate_ipc_request(&req).unwrap_err();
    assert!(
      err.contains("header value contains NUL/CR/LF"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn validate_ipc_request_rejects_overlong_request_body() {
    // Construct an encoded body string that will exceed the max decoded length estimate without
    // requiring us to actually base64-encode a huge buffer.
    let encoded_len = (IPC_MAX_REQUEST_BODY_BYTES / 3).saturating_add(1) * 4;
    let body_b64 = "A".repeat(encoded_len);
    let req = IpcRequest::FetchHttpRequest {
      req: IpcHttpRequest {
        fetch: IpcFetchRequest {
          url: "https://example.com/".to_string(),
          destination: FetchDestination::Fetch,
          referrer_url: None,
          client_origin: None,
          referrer_policy: ReferrerPolicy::NoReferrer,
          credentials_mode: FetchCredentialsMode::Omit,
        },
        method: "POST".to_string(),
        redirect: web_fetch::RequestRedirect::Follow,
        headers: vec![],
        body_b64: Some(body_b64),
      },
    };
    let err = validate_ipc_request(&req).unwrap_err();
    assert!(
      err.contains("request body exceeds max length"),
      "unexpected error: {err}"
    );
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcHttpCachePolicy {
  pub max_age: Option<u64>,
  pub s_maxage: Option<u64>,
  pub no_cache: bool,
  pub no_store: bool,
  pub must_revalidate: bool,
  pub expires_epoch_secs: Option<u64>,
  pub date_epoch_secs: Option<u64>,
  pub age: Option<u64>,
  pub stale_if_error: Option<u64>,
  pub stale_while_revalidate: Option<u64>,
  pub last_modified_epoch_secs: Option<u64>,
}

fn system_time_to_epoch_secs(time: std::time::SystemTime) -> Option<u64> {
  time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn epoch_secs_to_system_time(secs: u64) -> std::time::SystemTime {
  UNIX_EPOCH
    .checked_add(Duration::from_secs(secs))
    .unwrap_or(UNIX_EPOCH)
}

impl From<HttpCachePolicy> for IpcHttpCachePolicy {
  fn from(value: HttpCachePolicy) -> Self {
    Self::from(&value)
  }
}

impl From<&HttpCachePolicy> for IpcHttpCachePolicy {
  fn from(value: &HttpCachePolicy) -> Self {
    Self {
      max_age: value.max_age,
      s_maxage: value.s_maxage,
      no_cache: value.no_cache,
      no_store: value.no_store,
      must_revalidate: value.must_revalidate,
      expires_epoch_secs: value.expires.and_then(system_time_to_epoch_secs),
      date_epoch_secs: value.date.and_then(system_time_to_epoch_secs),
      age: value.age,
      stale_if_error: value.stale_if_error,
      stale_while_revalidate: value.stale_while_revalidate,
      last_modified_epoch_secs: value.last_modified.and_then(system_time_to_epoch_secs),
    }
  }
}

impl From<IpcHttpCachePolicy> for HttpCachePolicy {
  fn from(value: IpcHttpCachePolicy) -> Self {
    Self {
      max_age: value.max_age,
      s_maxage: value.s_maxage,
      no_cache: value.no_cache,
      no_store: value.no_store,
      must_revalidate: value.must_revalidate,
      expires: value.expires_epoch_secs.map(epoch_secs_to_system_time),
      date: value.date_epoch_secs.map(epoch_secs_to_system_time),
      age: value.age,
      stale_if_error: value.stale_if_error,
      stale_while_revalidate: value.stale_while_revalidate,
      last_modified: value
        .last_modified_epoch_secs
        .map(epoch_secs_to_system_time),
    }
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcFetchedResource {
  pub bytes_b64: String,
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub response_referrer_policy: Option<ReferrerPolicy>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<IpcHttpCachePolicy>,
  pub response_headers: Option<Vec<(String, String)>>,
}

impl From<FetchedResource> for IpcFetchedResource {
  fn from(value: FetchedResource) -> Self {
    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(&value.bytes);
    Self {
      bytes_b64,
      content_type: value.content_type,
      nosniff: value.nosniff,
      content_encoding: value.content_encoding,
      status: value.status,
      etag: value.etag,
      last_modified: value.last_modified,
      access_control_allow_origin: value.access_control_allow_origin,
      timing_allow_origin: value.timing_allow_origin,
      vary: value.vary,
      response_referrer_policy: value.response_referrer_policy,
      access_control_allow_credentials: value.access_control_allow_credentials,
      final_url: value.final_url,
      cache_policy: value.cache_policy.as_ref().map(IpcHttpCachePolicy::from),
      response_headers: value.response_headers,
    }
  }
}

impl TryFrom<IpcFetchedResource> for FetchedResource {
  type Error = String;

  fn try_from(value: IpcFetchedResource) -> std::result::Result<Self, Self::Error> {
    let upper = base64_decoded_len_upper_bound(&value.bytes_b64)
      .ok_or_else(|| "invalid base64 body length".to_string())?;
    if upper > IPC_MAX_BODY_BYTES {
      return Err(format!(
        "base64 body too large: decoded length upper bound {upper} exceeds hard limit {IPC_MAX_BODY_BYTES}"
      ));
    }
    let mut bytes = Vec::new();
    bytes
      .try_reserve_exact(upper)
      .map_err(|err| format!("base64 body allocation failed (len={upper}): {err:?}"))?;
    base64::engine::general_purpose::STANDARD
      .decode_vec(value.bytes_b64.as_bytes(), &mut bytes)
      .map_err(|err| format!("invalid base64 body: {err}"))?;
    Ok(FetchedResource {
      bytes,
      content_type: value.content_type,
      nosniff: value.nosniff,
      content_encoding: value.content_encoding,
      status: value.status,
      etag: value.etag,
      last_modified: value.last_modified,
      access_control_allow_origin: value.access_control_allow_origin,
      timing_allow_origin: value.timing_allow_origin,
      vary: value.vary,
      response_referrer_policy: value.response_referrer_policy,
      access_control_allow_credentials: value.access_control_allow_credentials,
      final_url: value.final_url,
      cache_policy: value.cache_policy.map(Into::into),
      response_headers: value.response_headers,
    })
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcCacheSourceMetadata {
  pub status: Option<u16>,
  #[serde(default)]
  pub nosniff: bool,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  #[serde(default)]
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<IpcHttpCachePolicy>,
}

impl IpcCacheSourceMetadata {
  fn from_fetched(source: &FetchedResource) -> Self {
    Self {
      status: source.status,
      nosniff: source.nosniff,
      etag: source.etag.clone(),
      last_modified: source.last_modified.clone(),
      access_control_allow_origin: source.access_control_allow_origin.clone(),
      timing_allow_origin: source.timing_allow_origin.clone(),
      vary: source.vary.clone(),
      access_control_allow_credentials: source.access_control_allow_credentials,
      final_url: source.final_url.clone(),
      cache_policy: source.cache_policy.as_ref().map(IpcHttpCachePolicy::from),
    }
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcFetchRequest {
  pub url: String,
  pub destination: FetchDestination,
  pub referrer_url: Option<String>,
  pub client_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub credentials_mode: FetchCredentialsMode,
}

impl IpcFetchRequest {
  pub fn from_fetch_request(req: FetchRequest<'_>) -> Self {
    Self {
      url: req.url.to_string(),
      destination: req.destination,
      referrer_url: req.referrer_url.map(str::to_string),
      client_origin: req.client_origin.cloned(),
      referrer_policy: req.referrer_policy,
      credentials_mode: req.credentials_mode,
    }
  }

  pub fn as_fetch_request(&self) -> FetchRequest<'_> {
    FetchRequest {
      url: &self.url,
      destination: self.destination,
      referrer_url: self.referrer_url.as_deref(),
      client_origin: self.client_origin.as_ref(),
      referrer_policy: self.referrer_policy,
      credentials_mode: self.credentials_mode,
    }
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcHttpRequest {
  pub fetch: IpcFetchRequest,
  pub method: String,
  pub redirect: web_fetch::RequestRedirect,
  pub headers: Vec<(String, String)>,
  pub body_b64: Option<String>,
}

impl IpcHttpRequest {
  pub fn from_http_request(req: HttpRequest<'_>) -> Self {
    let body_b64 = req
      .body
      .map(|body| base64::engine::general_purpose::STANDARD.encode(body));
    Self {
      fetch: IpcFetchRequest::from_fetch_request(req.fetch),
      method: req.method.to_string(),
      redirect: req.redirect,
      headers: req.headers.to_vec(),
      body_b64,
    }
  }

  pub fn decode_body(&self) -> std::result::Result<Option<Vec<u8>>, String> {
    let Some(encoded) = &self.body_b64 else {
      return Ok(None);
    };
    // Enforce a hard cap before decoding so we never allocate attacker-controlled request bodies.
    validate_ipc_body(encoded)?;
    base64::engine::general_purpose::STANDARD
      .decode(encoded.as_bytes())
      .map_err(|err| format!("invalid base64 request body: {err}"))
      .and_then(|bytes| {
        if bytes.len() > IPC_MAX_REQUEST_BODY_BYTES {
          return Err(format!(
            "request body exceeds max length ({} > {})",
            bytes.len(),
            IPC_MAX_REQUEST_BODY_BYTES
          ));
        }
        Ok(Some(bytes))
      })
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcError {
  pub message: String,
  pub content_type: Option<String>,
  pub status: Option<u16>,
  pub final_url: Option<String>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
}

impl From<Error> for IpcError {
  fn from(value: Error) -> Self {
    match value {
      Error::Resource(err) => Self {
        message: err.message,
        content_type: err.content_type,
        status: err.status,
        final_url: err.final_url,
        etag: err.etag,
        last_modified: err.last_modified,
      },
      other => Self {
        message: other.to_string(),
        content_type: None,
        status: None,
        final_url: None,
        etag: None,
        last_modified: None,
      },
    }
  }
}

impl IpcError {
  fn into_resource_error(self, url: &str) -> ResourceError {
    let mut err = ResourceError::new(url, self.message).with_content_type(self.content_type);
    if let Some(status) = self.status {
      err = err.with_status(status);
    }
    if let Some(final_url) = self.final_url {
      err = err.with_final_url(final_url);
    }
    err.with_validators(self.etag, self.last_modified)
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum IpcResult<T> {
  Ok(T),
  Err(IpcError),
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum IpcRequest {
  /// Authentication handshake. Must be the first message sent by a client after connecting.
  Hello { token: String },
  Fetch { url: String },
  FetchWithRequest { req: IpcFetchRequest },
  FetchWithRequestAndValidation {
    req: IpcFetchRequest,
    etag: Option<String>,
    last_modified: Option<String>,
  },
  FetchHttpRequest {
    req: IpcHttpRequest,
  },
  FetchPartialWithContext {
    kind: FetchContextKind,
    url: String,
    max_bytes: u64,
  },
  FetchPartialWithRequest {
    req: IpcFetchRequest,
    max_bytes: u64,
  },
  RequestHeaderValue {
    req: IpcFetchRequest,
    header_name: String,
  },
  CookieHeaderValue {
    url: String,
  },
  StoreCookieFromDocument {
    url: String,
    cookie_string: String,
  },
  ReadCacheArtifact {
    kind: FetchContextKind,
    url: String,
    artifact: CacheArtifactKind,
  },
  ReadCacheArtifactWithRequest {
    req: IpcFetchRequest,
    artifact: CacheArtifactKind,
  },
  WriteCacheArtifact {
    kind: FetchContextKind,
    url: String,
    artifact: CacheArtifactKind,
    bytes_b64: String,
    source: Option<IpcCacheSourceMetadata>,
  },
  WriteCacheArtifactWithRequest {
    req: IpcFetchRequest,
    artifact: CacheArtifactKind,
    bytes_b64: String,
    source: Option<IpcCacheSourceMetadata>,
  },
  RemoveCacheArtifact {
    kind: FetchContextKind,
    url: String,
    artifact: CacheArtifactKind,
  },
  RemoveCacheArtifactWithRequest {
    req: IpcFetchRequest,
    artifact: CacheArtifactKind,
  },
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum IpcResponse {
  /// Authentication handshake acknowledgement.
  HelloAck,
  Fetched(IpcResult<IpcFetchedResource>),
  MaybeFetched(IpcResult<Option<IpcFetchedResource>>),
  MaybeString(IpcResult<Option<String>>),
  Unit(IpcResult<()>),
}

fn validate_partial_content_range_start_zero(
  resource: &FetchedResource,
  requested_url: &str,
) -> Result<()> {
  if resource.status != Some(206) {
    return Ok(());
  }

  let Some(content_range) = resource.header_get_joined("content-range") else {
    return Err(super::response_resource_error(
      resource,
      requested_url,
      "received 206 Partial Content response without Content-Range header",
    ));
  };
  let parsed = parse_content_range(&content_range).ok_or_else(|| {
    super::response_resource_error(
      resource,
      requested_url,
      format!("invalid Content-Range header: {content_range:?}"),
    )
  })?;
  match parsed {
    ParsedContentRange::Range { start, .. } => {
      if start != 0 {
        return Err(super::response_resource_error(
          resource,
          requested_url,
          format!("Content-Range start mismatch: expected 0 but received {start}"),
        ));
      }
    }
    ParsedContentRange::Unsatisfied { size } => {
      return Err(super::response_resource_error(
        resource,
        requested_url,
        format!("received unsatisfied Content-Range for 206 response (size={size:?})"),
      ));
    }
  }

  Ok(())
}

struct IpcResourceFetcherInner {
  endpoint: String,
  stream: Mutex<TcpStream>,
  next_request_id: AtomicU64,
  last_response_chunked: AtomicBool,
}

/// Renderer-side [`ResourceFetcher`] proxy that forwards all fetch operations to a trusted network
/// process over an IPC socket.
///
/// The current implementation uses a single stream guarded by a mutex, so requests are processed
/// sequentially. This keeps the IPC framing simple while remaining safe for concurrent callers.
#[derive(Clone)]
pub struct IpcResourceFetcher {
  inner: Arc<IpcResourceFetcherInner>,
}

impl std::fmt::Debug for IpcResourceFetcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IpcResourceFetcher")
      .field("endpoint", &self.inner.endpoint)
      .finish_non_exhaustive()
  }
}

impl IpcResourceFetcher {
  /// Connect to the network process at `socket_name`.
  ///
  /// `socket_name` is currently interpreted as a TCP address (e.g. `"127.0.0.1:1234"`).
  pub fn new(socket_name: impl Into<String>) -> Result<Self> {
    let token = std::env::var(IPC_AUTH_TOKEN_ENV).map_err(|_| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!(
          "missing IPC auth token: set {IPC_AUTH_TOKEN_ENV} or use IpcResourceFetcher::new_with_auth_token"
        ),
      ))
    })?;
    Self::new_with_auth_token(socket_name, token)
  }

  /// Connect to the network process at `socket_name`, authenticating with `auth_token`.
  pub fn new_with_auth_token(
    socket_name: impl Into<String>,
    auth_token: impl Into<String>,
  ) -> Result<Self> {
    let auth_token = auth_token.into();
    if auth_token.is_empty() {
      return Err(Error::Resource(ResourceError::new(
        "<ipc>",
        "IPC auth token is empty".to_string(),
      )));
    }
    if auth_token.len() > IPC_MAX_AUTH_TOKEN_BYTES {
      return Err(Error::Resource(ResourceError::new(
        "<ipc>",
        format!(
          "IPC auth token too large: {} bytes (max {})",
          auth_token.len(),
          IPC_MAX_AUTH_TOKEN_BYTES
        ),
      )));
    }

    let endpoint = socket_name.into();
    let mut stream = TcpStream::connect(&endpoint).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!(
          "failed to connect to network process at {endpoint}: {err} (is the network process running?)"
        ),
      ))
    })?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IPC_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IPC_IO_TIMEOUT));

    // Authenticate immediately; the network process must ignore any traffic until it receives the
    // correct token.
    let hello = IpcRequest::Hello { token: auth_token };
    let payload = serde_json::to_vec(&hello).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!("failed to serialize IPC hello request: {err}"),
      ))
    })?;
    write_ipc_frame(&mut stream, &payload, IPC_MAX_INBOUND_FRAME_BYTES).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!("IPC hello write to network process at {endpoint} failed: {err}"),
      ))
    })?;
    let response_bytes = read_ipc_frame(&mut stream, IPC_MAX_OUTBOUND_FRAME_BYTES).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!("IPC hello read from network process at {endpoint} failed: {err}"),
      ))
    })?;
    let response: IpcResponse = serde_json::from_slice(&response_bytes).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!("failed to deserialize IPC hello response: {err}"),
      ))
    })?;
    if !matches!(response, IpcResponse::HelloAck) {
      return Err(Error::Resource(ResourceError::new(
        "<ipc>",
        format!("unexpected IPC hello response: {response:?}"),
      )));
    }

    Ok(Self {
      inner: Arc::new(IpcResourceFetcherInner {
        endpoint,
        stream: Mutex::new(stream),
        next_request_id: AtomicU64::new(1),
        last_response_chunked: AtomicBool::new(false),
      }),
    })
  }

  /// Test hook: returns `true` when the last fetch-like RPC used the chunked response path.
  pub fn last_response_was_chunked(&self) -> bool {
    self.inner.last_response_chunked.load(Ordering::Relaxed)
  }

  fn rpc(&self, url: &str, request: &IpcRequest) -> Result<RpcReply> {
    if let Err(err) = validate_ipc_request(request) {
      return Err(Error::Resource(ResourceError::new(
        url,
        format!("IPC request rejected by local validation: {err}"),
      )));
    }

    let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
    let envelope = BrowserToNetwork {
      id: request_id,
      request: request.clone(),
    };
    let payload = serde_json::to_vec(&envelope).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!("failed to serialize IPC request: {err}"),
      ))
    })?;

    let mut guard = match self.inner.stream.lock() {
      Ok(guard) => guard,
      Err(poisoned) => poisoned.into_inner(),
    };

    if let Err(err) = write_ipc_frame(&mut *guard, &payload, IPC_MAX_INBOUND_FRAME_BYTES) {
      let _ = guard.shutdown(std::net::Shutdown::Both);
      return Err(Error::Resource(ResourceError::new(
        url,
        format!(
          "IPC write to network process at {} failed: {err}",
          self.inner.endpoint
        ),
      )));
    }

    let first_frame = match read_ipc_frame(&mut *guard, IPC_MAX_OUTBOUND_FRAME_BYTES) {
      Ok(bytes) => bytes,
      Err(err) => {
        let _ = guard.shutdown(std::net::Shutdown::Both);
        return Err(Error::Resource(ResourceError::new(
          url,
          format!(
            "IPC read from network process at {} failed: {err}",
            self.inner.endpoint
          ),
        )));
      }
    };

    let first_msg: NetworkToBrowser = match serde_json::from_slice(&first_frame) {
      Ok(msg) => msg,
      Err(err) => {
        let _ = guard.shutdown(std::net::Shutdown::Both);
        return Err(Error::Resource(ResourceError::new(
          url,
          format!("failed to deserialize IPC response: {err}"),
        )));
      }
    };

    match first_msg {
      NetworkToBrowser::Response { id, response } => {
        if id != request_id {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          return Err(Error::Resource(ResourceError::new(
            url,
            format!("IPC protocol error: response id {id} did not match request id {request_id}"),
          )));
        }
        self
          .inner
          .last_response_chunked
          .store(false, Ordering::Relaxed);
        Ok(RpcReply::Response(response))
      }
      NetworkToBrowser::FetchErr { id, err } => {
        if id != request_id {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          return Err(Error::Resource(ResourceError::new(
            url,
            format!(
              "IPC protocol error: fetch error id {id} did not match request id {request_id}"
            ),
          )));
        }
        self
          .inner
          .last_response_chunked
          .store(true, Ordering::Relaxed);
        Ok(RpcReply::ChunkedFetch(IpcResult::Err(err)))
      }
      NetworkToBrowser::FetchStart {
        id,
        meta,
        total_len,
      } => {
        if id != request_id {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          return Err(Error::Resource(ResourceError::new(
            url,
            format!(
              "IPC protocol error: fetch start id {id} did not match request id {request_id}"
            ),
          )));
        }
        if total_len > IPC_MAX_BODY_BYTES {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          return Err(Error::Resource(ResourceError::new(
            url,
            format!(
              "IPC protocol error: chunked body total_len {total_len} exceeds hard limit {IPC_MAX_BODY_BYTES}"
            ),
          )));
        }

        self
          .inner
          .last_response_chunked
          .store(true, Ordering::Relaxed);

        let mut body = Vec::new();
        body.try_reserve_exact(total_len).map_err(|err| {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          Error::Resource(ResourceError::new(
            url,
            format!("IPC body allocation failed (len={total_len}): {err:?}"),
          ))
        })?;
        loop {
          let frame = match read_ipc_frame(&mut *guard, IPC_MAX_OUTBOUND_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(err) => {
              let _ = guard.shutdown(std::net::Shutdown::Both);
              return Err(Error::Resource(ResourceError::new(
                url,
                format!(
                  "IPC read from network process at {} failed: {err}",
                  self.inner.endpoint
                ),
              )));
            }
          };
          let msg: NetworkToBrowser = match serde_json::from_slice(&frame) {
            Ok(msg) => msg,
            Err(err) => {
              let _ = guard.shutdown(std::net::Shutdown::Both);
              return Err(Error::Resource(ResourceError::new(
                url,
                format!("failed to deserialize IPC chunked response: {err}"),
              )));
            }
          };
          match msg {
            NetworkToBrowser::FetchBodyChunk { id, bytes_b64 } => {
              if id != request_id {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  format!(
                    "IPC protocol error: fetch chunk id {id} did not match request id {request_id}"
                  ),
                )));
              }
              let upper = base64_decoded_len_upper_bound(&bytes_b64).ok_or_else(|| {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                Error::Resource(ResourceError::new(
                  url,
                  "IPC protocol error: invalid base64 chunk length".to_string(),
                ))
              })?;
              if upper > IPC_CHUNK_MAX_BYTES {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  format!(
                    "IPC protocol error: decoded chunk length upper bound {upper} exceeds chunk max {IPC_CHUNK_MAX_BYTES}"
                  ),
                )));
              }

              let decoded = base64::engine::general_purpose::STANDARD
                .decode(bytes_b64.as_bytes())
                .map_err(|err| {
                  let _ = guard.shutdown(std::net::Shutdown::Both);
                  Error::Resource(ResourceError::new(
                    url,
                    format!("IPC protocol error: invalid base64 body chunk: {err}"),
                  ))
                })?;
              if decoded.len() > IPC_CHUNK_MAX_BYTES {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  format!(
                    "IPC protocol error: decoded chunk length {} exceeds chunk max {IPC_CHUNK_MAX_BYTES}",
                    decoded.len()
                  ),
                )));
              }
              if body.len() + decoded.len() > total_len {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  "IPC protocol error: received more chunk bytes than advertised total_len"
                    .to_string(),
                )));
              }
              body.extend_from_slice(&decoded);
            }
            NetworkToBrowser::FetchEnd { id } => {
              if id != request_id {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  format!(
                    "IPC protocol error: fetch end id {id} did not match request id {request_id}"
                  ),
                )));
              }
              break;
            }
            NetworkToBrowser::FetchErr { id, err } => {
              if id != request_id {
                let _ = guard.shutdown(std::net::Shutdown::Both);
                return Err(Error::Resource(ResourceError::new(
                  url,
                  format!(
                    "IPC protocol error: fetch error id {id} did not match request id {request_id}"
                  ),
                )));
              }
              return Ok(RpcReply::ChunkedFetch(IpcResult::Err(err)));
            }
            other => {
              let _ = guard.shutdown(std::net::Shutdown::Both);
              return Err(Error::Resource(ResourceError::new(
                url,
                format!("IPC protocol error: unexpected message during chunked fetch: {other:?}"),
              )));
            }
          }
        }

        let actual_len = body.len();
        if actual_len != total_len {
          let _ = guard.shutdown(std::net::Shutdown::Both);
          return Err(Error::Resource(ResourceError::new(
            url,
            format!(
              "IPC protocol error: chunked body length {actual_len} did not match total_len {total_len}"
            ),
          )));
        }

        Ok(RpcReply::ChunkedFetch(IpcResult::Ok((meta, body))))
      }
      other => {
        let _ = guard.shutdown(std::net::Shutdown::Both);
        Err(Error::Resource(ResourceError::new(
          url,
          format!("IPC protocol error: unexpected response message: {other:?}"),
        )))
      }
    }
  }

  fn rpc_fetched(&self, url: &str, request: &IpcRequest) -> Result<FetchedResource> {
    match self.rpc(url, request)? {
      RpcReply::Response(IpcResponse::Fetched(IpcResult::Ok(res))) => {
        FetchedResource::try_from(res).map_err(|err| {
          Error::Resource(ResourceError::new(
            url,
            format!("failed to decode IPC fetched resource: {err}"),
          ))
        })
      }
      RpcReply::Response(IpcResponse::Fetched(IpcResult::Err(err))) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      RpcReply::ChunkedFetch(IpcResult::Ok((meta, bytes))) => Ok(meta.into_fetched_resource(bytes)),
      RpcReply::ChunkedFetch(IpcResult::Err(err)) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      RpcReply::Response(other) => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response for fetch: {other:?}"),
      ))),
    }
  }

  fn rpc_maybe_fetched(&self, url: &str, request: &IpcRequest) -> Result<Option<FetchedResource>> {
    match self.rpc(url, request)? {
      RpcReply::Response(IpcResponse::MaybeFetched(IpcResult::Ok(Some(res)))) => {
        let res = FetchedResource::try_from(res).map_err(|err| {
          Error::Resource(ResourceError::new(
            url,
            format!("failed to decode IPC fetched resource: {err}"),
          ))
        })?;
        Ok(Some(res))
      }
      RpcReply::Response(IpcResponse::MaybeFetched(IpcResult::Ok(None))) => Ok(None),
      RpcReply::Response(IpcResponse::MaybeFetched(IpcResult::Err(_))) => Ok(None),
      RpcReply::ChunkedFetch(IpcResult::Ok((meta, bytes))) => {
        Ok(Some(meta.into_fetched_resource(bytes)))
      }
      RpcReply::ChunkedFetch(IpcResult::Err(_)) => Ok(None),
      RpcReply::Response(other) => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
      ))),
    }
  }

  fn rpc_maybe_string(&self, url: &str, request: &IpcRequest) -> Result<Option<String>> {
    match self.rpc(url, request)? {
      RpcReply::Response(IpcResponse::MaybeString(IpcResult::Ok(value))) => Ok(value),
      RpcReply::Response(IpcResponse::MaybeString(IpcResult::Err(err))) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      RpcReply::Response(other) => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
      ))),
      RpcReply::ChunkedFetch(_) => Err(Error::Resource(ResourceError::new(
        url,
        "unexpected chunked fetch response for string RPC".to_string(),
      ))),
    }
  }

  fn rpc_unit(&self, url: &str, request: &IpcRequest) -> Result<()> {
    match self.rpc(url, request)? {
      RpcReply::Response(IpcResponse::Unit(IpcResult::Ok(()))) => Ok(()),
      RpcReply::Response(IpcResponse::Unit(IpcResult::Err(err))) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      RpcReply::Response(other) => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
      ))),
      RpcReply::ChunkedFetch(_) => Err(Error::Resource(ResourceError::new(
        url,
        "unexpected chunked fetch response for unit RPC".to_string(),
      ))),
    }
  }

  fn send_best_effort(&self, url: &str, request: &IpcRequest) {
    let _ = self.rpc_unit(url, request);
  }
}

impl ResourceFetcher for IpcResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.rpc_fetched(
      url,
      &IpcRequest::Fetch {
        url: url.to_string(),
      },
    )
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    let url = req.url;
    self.rpc_fetched(
      url,
      &IpcRequest::FetchWithRequest {
        req: IpcFetchRequest::from_fetch_request(req),
      },
    )
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    let url = req.url;
    self.rpc_fetched(
      url,
      &IpcRequest::FetchWithRequestAndValidation {
        req: IpcFetchRequest::from_fetch_request(req),
        etag: etag.map(str::to_string),
        last_modified: last_modified.map(str::to_string),
      },
    )
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    let url = req.fetch.url;
    self.rpc_fetched(
      url,
      &IpcRequest::FetchHttpRequest {
        req: IpcHttpRequest::from_http_request(req),
      },
    )
  }

  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let res = self.rpc_fetched(
      url,
      &IpcRequest::FetchPartialWithContext {
        kind,
        url: url.to_string(),
        max_bytes: max_bytes as u64,
      },
    )?;
    validate_partial_content_range_start_zero(&res, url)?;
    Ok(res)
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let url = req.url;
    let res = self.rpc_fetched(
      url,
      &IpcRequest::FetchPartialWithRequest {
        req: IpcFetchRequest::from_fetch_request(req),
        max_bytes: max_bytes as u64,
      },
    )?;
    validate_partial_content_range_start_zero(&res, url)?;
    Ok(res)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    let url = req.url;
    let request = IpcRequest::RequestHeaderValue {
      req: IpcFetchRequest::from_fetch_request(req),
      header_name: header_name.to_string(),
    };
    match self.rpc_maybe_string(url, &request) {
      Ok(value) => value,
      Err(_) => None,
    }
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    // Match `HttpFetcher` semantics: invalid URLs yield `None` (cookie state not observable).
    let _ = Url::parse(url).ok()?;
    let request = IpcRequest::CookieHeaderValue {
      url: url.to_string(),
    };
    match self.rpc_maybe_string(url, &request) {
      Ok(Some(value)) => Some(value),
      // The network process should use an empty string to represent "no cookies", but treat
      // missing values deterministically too so callers can compute stable `document.cookie`.
      Ok(None) => Some(String::new()),
      Err(_) => None,
    }
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    // Match the per-cookie size limit enforced by `HttpFetcher`.
    if cookie_string.len() > MAX_COOKIE_BYTES {
      return;
    }
    if Url::parse(url).is_err() {
      return;
    }
    self.send_best_effort(
      url,
      &IpcRequest::StoreCookieFromDocument {
        url: url.to_string(),
        cookie_string: cookie_string.to_string(),
      },
    );
  }

  fn read_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    let request = IpcRequest::ReadCacheArtifact {
      kind,
      url: url.to_string(),
      artifact,
    };
    match self.rpc_maybe_fetched(url, &request) {
      Ok(value) => value,
      Err(_) => None,
    }
  }

  fn read_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    let url = req.url;
    let request = IpcRequest::ReadCacheArtifactWithRequest {
      req: IpcFetchRequest::from_fetch_request(req),
      artifact,
    };
    match self.rpc_maybe_fetched(url, &request) {
      Ok(value) => value,
      Err(_) => None,
    }
  }

  fn write_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let request = IpcRequest::WriteCacheArtifact {
      kind,
      url: url.to_string(),
      artifact,
      bytes_b64,
      source: source.map(IpcCacheSourceMetadata::from_fetched),
    };
    self.send_best_effort(url, &request);
  }

  fn write_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    let url = req.url;
    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let request = IpcRequest::WriteCacheArtifactWithRequest {
      req: IpcFetchRequest::from_fetch_request(req),
      artifact,
      bytes_b64,
      source: source.map(IpcCacheSourceMetadata::from_fetched),
    };
    self.send_best_effort(url, &request);
  }

  fn remove_cache_artifact(&self, kind: FetchContextKind, url: &str, artifact: CacheArtifactKind) {
    let request = IpcRequest::RemoveCacheArtifact {
      kind,
      url: url.to_string(),
      artifact,
    };
    self.send_best_effort(url, &request);
  }

  fn remove_cache_artifact_with_request(&self, req: FetchRequest<'_>, artifact: CacheArtifactKind) {
    let url = req.url;
    let request = IpcRequest::RemoveCacheArtifactWithRequest {
      req: IpcFetchRequest::from_fetch_request(req),
      artifact,
    };
    self.send_best_effort(url, &request);
  }
}
