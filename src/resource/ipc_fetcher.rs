use super::{
  web_fetch, CacheArtifactKind, DocumentOrigin, FetchContextKind, FetchCredentialsMode,
  FetchDestination, FetchRequest, FetchedResource, HttpCachePolicy, HttpRequest, ReferrerPolicy,
  ResourceFetcher,
};
use crate::error::{Error, ResourceError, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::UNIX_EPOCH;

const IPC_FRAME_LEN_BYTES: usize = 4;

/// Maximum frame size accepted by the IPC client.
///
/// Responses include base64-encoded bodies, so this limit must exceed the underlying fetcher's
/// `max_response_bytes` budget. The default `ResourcePolicy` cap is 50MB, which expands to ~67MB
/// when base64-encoded; keep a comfortable margin.
const IPC_MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

fn write_ipc_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
  if payload.len() > u32::MAX as usize {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "IPC frame too large",
    ));
  }
  let len = (payload.len() as u32).to_le_bytes();
  stream.write_all(&len)?;
  stream.write_all(payload)?;
  stream.flush()?;
  Ok(())
}

fn read_ipc_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
  let mut len_buf = [0u8; IPC_FRAME_LEN_BYTES];
  stream.read_exact(&mut len_buf)?;
  let len = u32::from_le_bytes(len_buf) as usize;
  if len > IPC_MAX_FRAME_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame too large: {len} bytes"),
    ));
  }
  let mut buf = vec![0u8; len];
  stream.read_exact(&mut buf)?;
  Ok(buf)
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let bytes = base64::engine::general_purpose::STANDARD
      .decode(value.bytes_b64.as_bytes())
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
    base64::engine::general_purpose::STANDARD
      .decode(encoded.as_bytes())
      .map(Some)
      .map_err(|err| format!("invalid base64 request body: {err}"))
  }
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub enum IpcResult<T> {
  Ok(T),
  Err(IpcError),
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
  Fetch {
    url: String,
  },
  FetchWithRequest {
    req: IpcFetchRequest,
  },
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
pub enum IpcResponse {
  Fetched(IpcResult<IpcFetchedResource>),
  MaybeFetched(IpcResult<Option<IpcFetchedResource>>),
  MaybeString(IpcResult<Option<String>>),
  Unit(IpcResult<()>),
}

struct IpcResourceFetcherInner {
  endpoint: String,
  stream: Mutex<TcpStream>,
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

impl IpcResourceFetcher {
  /// Connect to the network process at `socket_name`.
  ///
  /// `socket_name` is currently interpreted as a TCP address (e.g. `"127.0.0.1:1234"`).
  pub fn new(socket_name: impl Into<String>) -> Result<Self> {
    let endpoint = socket_name.into();
    let stream = TcpStream::connect(&endpoint).map_err(|err| {
      Error::Resource(ResourceError::new(
        "<ipc>",
        format!(
          "failed to connect to network process at {endpoint}: {err} (is the network process running?)"
        ),
      ))
    })?;
    let _ = stream.set_nodelay(true);
    Ok(Self {
      inner: Arc::new(IpcResourceFetcherInner {
        endpoint,
        stream: Mutex::new(stream),
      }),
    })
  }

  fn rpc(&self, url: &str, request: &IpcRequest) -> Result<IpcResponse> {
    let payload = serde_json::to_vec(request).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!("failed to serialize IPC request: {err}"),
      ))
    })?;

    let mut guard = match self.inner.stream.lock() {
      Ok(guard) => guard,
      Err(poisoned) => poisoned.into_inner(),
    };

    write_ipc_frame(&mut guard, &payload).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!(
          "IPC write to network process at {} failed: {err}",
          self.inner.endpoint
        ),
      ))
    })?;

    let response_bytes = read_ipc_frame(&mut guard).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!(
          "IPC read from network process at {} failed: {err}",
          self.inner.endpoint
        ),
      ))
    })?;

    serde_json::from_slice(&response_bytes).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!("failed to deserialize IPC response: {err}"),
      ))
    })
  }

  fn rpc_fetched(&self, url: &str, request: &IpcRequest) -> Result<FetchedResource> {
    match self.rpc(url, request)? {
      IpcResponse::Fetched(IpcResult::Ok(res)) => FetchedResource::try_from(res).map_err(|err| {
        Error::Resource(ResourceError::new(
          url,
          format!("failed to decode IPC fetched resource: {err}"),
        ))
      }),
      IpcResponse::Fetched(IpcResult::Err(err)) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      other => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response for fetch: {other:?}"),
      ))),
    }
  }

  fn rpc_maybe_fetched(&self, url: &str, request: &IpcRequest) -> Result<Option<FetchedResource>> {
    match self.rpc(url, request)? {
      IpcResponse::MaybeFetched(IpcResult::Ok(Some(res))) => {
        let res = FetchedResource::try_from(res).map_err(|err| {
          Error::Resource(ResourceError::new(
            url,
            format!("failed to decode IPC fetched resource: {err}"),
          ))
        })?;
        Ok(Some(res))
      }
      IpcResponse::MaybeFetched(IpcResult::Ok(None)) => Ok(None),
      IpcResponse::MaybeFetched(IpcResult::Err(_)) => Ok(None),
      other => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
      ))),
    }
  }

  fn rpc_maybe_string(&self, url: &str, request: &IpcRequest) -> Result<Option<String>> {
    match self.rpc(url, request)? {
      IpcResponse::MaybeString(IpcResult::Ok(value)) => Ok(value),
      IpcResponse::MaybeString(IpcResult::Err(err)) => {
        Err(Error::Resource(err.into_resource_error(url)))
      }
      other => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
      ))),
    }
  }

  fn rpc_unit(&self, url: &str, request: &IpcRequest) -> Result<()> {
    match self.rpc(url, request)? {
      IpcResponse::Unit(IpcResult::Ok(())) => Ok(()),
      IpcResponse::Unit(IpcResult::Err(err)) => Err(Error::Resource(err.into_resource_error(url))),
      other => Err(Error::Resource(ResourceError::new(
        url,
        format!("unexpected IPC response: {other:?}"),
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
    self.rpc_fetched(
      url,
      &IpcRequest::FetchPartialWithContext {
        kind,
        url: url.to_string(),
        max_bytes: max_bytes as u64,
      },
    )
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let url = req.url;
    self.rpc_fetched(
      url,
      &IpcRequest::FetchPartialWithRequest {
        req: IpcFetchRequest::from_fetch_request(req),
        max_bytes: max_bytes as u64,
      },
    )
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
    let request = IpcRequest::CookieHeaderValue {
      url: url.to_string(),
    };
    match self.rpc_maybe_string(url, &request) {
      Ok(value) => value,
      Err(_) => None,
    }
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
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
