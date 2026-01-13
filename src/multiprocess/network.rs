use crate::error::{Error as RenderError, ResourceError};
use crate::resource::{
  self, FetchContextKind, FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource,
  HttpFetcher, HttpRequest, ReferrerPolicy, ResourceFetcher,
};
use serde::{Deserialize, Serialize};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type NetworkResult<T> = std::result::Result<T, NetworkError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkError {
  pub url: String,
  pub message: String,
  pub status: Option<u16>,
  pub final_url: Option<String>,
  pub content_type: Option<String>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
}

impl NetworkError {
  fn from_render_error(url: &str, err: RenderError) -> Self {
    match err {
      RenderError::Resource(resource) => Self::from_resource_error(resource),
      other => Self {
        url: url.to_string(),
        message: other.to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      },
    }
  }

  fn from_resource_error(err: ResourceError) -> Self {
    Self {
      url: err.url,
      message: err.message,
      status: err.status,
      final_url: err.final_url,
      content_type: err.content_type,
      etag: err.etag,
      last_modified: err.last_modified,
    }
  }

  fn service_disconnected() -> Self {
    Self {
      url: "<network-service>".to_string(),
      message: "network service disconnected".to_string(),
      status: None,
      final_url: None,
      content_type: None,
      etag: None,
      last_modified: None,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkFetchRequest {
  pub url: String,
  pub destination: NetworkFetchDestination,
  pub referrer_url: Option<String>,
  /// Origin URL string for the initiating client (e.g. "https://example.com/").
  pub client_origin: Option<String>,
  pub referrer_policy: NetworkReferrerPolicy,
  pub credentials_mode: NetworkFetchCredentialsMode,
}

impl NetworkFetchRequest {
  pub fn new(url: impl Into<String>, destination: NetworkFetchDestination) -> Self {
    Self {
      url: url.into(),
      destination,
      referrer_url: None,
      client_origin: None,
      referrer_policy: NetworkReferrerPolicy::EmptyString,
      credentials_mode: destination.default_credentials_mode(),
    }
  }

  pub fn with_referrer_url(mut self, referrer_url: impl Into<String>) -> Self {
    self.referrer_url = Some(referrer_url.into());
    self
  }

  pub fn with_client_origin(mut self, client_origin: impl Into<String>) -> Self {
    self.client_origin = Some(client_origin.into());
    self
  }

  pub fn with_referrer_policy(mut self, policy: NetworkReferrerPolicy) -> Self {
    self.referrer_policy = policy;
    self
  }

  pub fn with_credentials_mode(mut self, mode: NetworkFetchCredentialsMode) -> Self {
    self.credentials_mode = mode;
    self
  }

  fn with_fetch_request<T>(&self, f: impl FnOnce(FetchRequest<'_>) -> T) -> T {
    let destination: FetchDestination = self.destination.into();
    let client_origin = self
      .client_origin
      .as_deref()
      .and_then(resource::origin_from_url);
    let fetch = FetchRequest {
      url: &self.url,
      destination,
      referrer_url: self.referrer_url.as_deref(),
      client_origin: client_origin.as_ref(),
      referrer_policy: self.referrer_policy.into(),
      credentials_mode: self.credentials_mode.into(),
    };
    f(fetch)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkHttpRequest {
  pub fetch: NetworkFetchRequest,
  pub method: String,
  pub redirect: NetworkRequestRedirect,
  pub headers: Vec<(String, String)>,
  pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkFetchedResource {
  pub bytes: Vec<u8>,
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub response_referrer_policy: Option<NetworkReferrerPolicy>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<NetworkHttpCachePolicy>,
  pub response_headers: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkHttpCachePolicy {
  pub max_age: Option<u64>,
  pub s_maxage: Option<u64>,
  pub no_cache: bool,
  pub no_store: bool,
  pub must_revalidate: bool,
  pub expires: Option<u64>,
  pub date: Option<u64>,
  pub age: Option<u64>,
  pub stale_if_error: Option<u64>,
  pub stale_while_revalidate: Option<u64>,
  pub last_modified: Option<u64>,
}

fn system_time_to_epoch_secs(time: SystemTime) -> Option<u64> {
  time
    .duration_since(UNIX_EPOCH)
    .ok()
    .map(|duration| duration.as_secs())
}

impl From<FetchedResource> for NetworkFetchedResource {
  fn from(value: FetchedResource) -> Self {
    Self {
      bytes: value.bytes,
      content_type: value.content_type,
      nosniff: value.nosniff,
      content_encoding: value.content_encoding,
      status: value.status,
      etag: value.etag,
      last_modified: value.last_modified,
      access_control_allow_origin: value.access_control_allow_origin,
      timing_allow_origin: value.timing_allow_origin,
      vary: value.vary,
      response_referrer_policy: value.response_referrer_policy.map(Into::into),
      access_control_allow_credentials: value.access_control_allow_credentials,
      final_url: value.final_url,
      cache_policy: value.cache_policy.map(|policy| NetworkHttpCachePolicy {
        max_age: policy.max_age,
        s_maxage: policy.s_maxage,
        no_cache: policy.no_cache,
        no_store: policy.no_store,
        must_revalidate: policy.must_revalidate,
        expires: policy.expires.and_then(system_time_to_epoch_secs),
        date: policy.date.and_then(system_time_to_epoch_secs),
        age: policy.age,
        stale_if_error: policy.stale_if_error,
        stale_while_revalidate: policy.stale_while_revalidate,
        last_modified: policy.last_modified.and_then(system_time_to_epoch_secs),
      }),
      response_headers: value.response_headers,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetworkFetchDestination {
  Document,
  DocumentNoUser,
  Iframe,
  Style,
  StyleCors,
  Script,
  ScriptCors,
  Image,
  ImageCors,
  Video,
  VideoCors,
  Audio,
  AudioCors,
  Font,
  Other,
  Fetch,
}

impl From<NetworkFetchDestination> for FetchDestination {
  fn from(value: NetworkFetchDestination) -> Self {
    match value {
      NetworkFetchDestination::Document => FetchDestination::Document,
      NetworkFetchDestination::DocumentNoUser => FetchDestination::DocumentNoUser,
      NetworkFetchDestination::Iframe => FetchDestination::Iframe,
      NetworkFetchDestination::Style => FetchDestination::Style,
      NetworkFetchDestination::StyleCors => FetchDestination::StyleCors,
      NetworkFetchDestination::Script => FetchDestination::Script,
      NetworkFetchDestination::ScriptCors => FetchDestination::ScriptCors,
      NetworkFetchDestination::Image => FetchDestination::Image,
      NetworkFetchDestination::ImageCors => FetchDestination::ImageCors,
      NetworkFetchDestination::Video => FetchDestination::Video,
      NetworkFetchDestination::VideoCors => FetchDestination::VideoCors,
      NetworkFetchDestination::Audio => FetchDestination::Audio,
      NetworkFetchDestination::AudioCors => FetchDestination::AudioCors,
      NetworkFetchDestination::Font => FetchDestination::Font,
      NetworkFetchDestination::Other => FetchDestination::Other,
      NetworkFetchDestination::Fetch => FetchDestination::Fetch,
    }
  }
}

impl From<FetchDestination> for NetworkFetchDestination {
  fn from(value: FetchDestination) -> Self {
    match value {
      FetchDestination::Document => Self::Document,
      FetchDestination::DocumentNoUser => Self::DocumentNoUser,
      FetchDestination::Iframe => Self::Iframe,
      FetchDestination::Style => Self::Style,
      FetchDestination::StyleCors => Self::StyleCors,
      FetchDestination::Script => Self::Script,
      FetchDestination::ScriptCors => Self::ScriptCors,
      FetchDestination::Image => Self::Image,
      FetchDestination::ImageCors => Self::ImageCors,
      FetchDestination::Video => Self::Video,
      FetchDestination::VideoCors => Self::VideoCors,
      FetchDestination::Audio => Self::Audio,
      FetchDestination::AudioCors => Self::AudioCors,
      FetchDestination::Font => Self::Font,
      FetchDestination::Other => Self::Other,
      FetchDestination::Fetch => Self::Fetch,
    }
  }
}

impl NetworkFetchDestination {
  const fn default_credentials_mode(self) -> NetworkFetchCredentialsMode {
    match self {
      // CORS-mode requests default to `same-origin` credentials.
      NetworkFetchDestination::StyleCors
      | NetworkFetchDestination::ScriptCors
      | NetworkFetchDestination::ImageCors
      | NetworkFetchDestination::VideoCors
      | NetworkFetchDestination::AudioCors
      | NetworkFetchDestination::Font
      | NetworkFetchDestination::Fetch => NetworkFetchCredentialsMode::SameOrigin,
      _ => NetworkFetchCredentialsMode::Include,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetworkFetchCredentialsMode {
  Omit,
  SameOrigin,
  Include,
}

impl From<NetworkFetchCredentialsMode> for FetchCredentialsMode {
  fn from(value: NetworkFetchCredentialsMode) -> Self {
    match value {
      NetworkFetchCredentialsMode::Omit => FetchCredentialsMode::Omit,
      NetworkFetchCredentialsMode::SameOrigin => FetchCredentialsMode::SameOrigin,
      NetworkFetchCredentialsMode::Include => FetchCredentialsMode::Include,
    }
  }
}

impl From<FetchCredentialsMode> for NetworkFetchCredentialsMode {
  fn from(value: FetchCredentialsMode) -> Self {
    match value {
      FetchCredentialsMode::Omit => Self::Omit,
      FetchCredentialsMode::SameOrigin => Self::SameOrigin,
      FetchCredentialsMode::Include => Self::Include,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetworkReferrerPolicy {
  EmptyString,
  NoReferrer,
  NoReferrerWhenDowngrade,
  Origin,
  OriginWhenCrossOrigin,
  SameOrigin,
  StrictOrigin,
  StrictOriginWhenCrossOrigin,
  UnsafeUrl,
}

impl From<NetworkReferrerPolicy> for ReferrerPolicy {
  fn from(value: NetworkReferrerPolicy) -> Self {
    match value {
      NetworkReferrerPolicy::EmptyString => ReferrerPolicy::EmptyString,
      NetworkReferrerPolicy::NoReferrer => ReferrerPolicy::NoReferrer,
      NetworkReferrerPolicy::NoReferrerWhenDowngrade => ReferrerPolicy::NoReferrerWhenDowngrade,
      NetworkReferrerPolicy::Origin => ReferrerPolicy::Origin,
      NetworkReferrerPolicy::OriginWhenCrossOrigin => ReferrerPolicy::OriginWhenCrossOrigin,
      NetworkReferrerPolicy::SameOrigin => ReferrerPolicy::SameOrigin,
      NetworkReferrerPolicy::StrictOrigin => ReferrerPolicy::StrictOrigin,
      NetworkReferrerPolicy::StrictOriginWhenCrossOrigin => ReferrerPolicy::StrictOriginWhenCrossOrigin,
      NetworkReferrerPolicy::UnsafeUrl => ReferrerPolicy::UnsafeUrl,
    }
  }
}

impl From<ReferrerPolicy> for NetworkReferrerPolicy {
  fn from(value: ReferrerPolicy) -> Self {
    match value {
      ReferrerPolicy::EmptyString => Self::EmptyString,
      ReferrerPolicy::NoReferrer => Self::NoReferrer,
      ReferrerPolicy::NoReferrerWhenDowngrade => Self::NoReferrerWhenDowngrade,
      ReferrerPolicy::Origin => Self::Origin,
      ReferrerPolicy::OriginWhenCrossOrigin => Self::OriginWhenCrossOrigin,
      ReferrerPolicy::SameOrigin => Self::SameOrigin,
      ReferrerPolicy::StrictOrigin => Self::StrictOrigin,
      ReferrerPolicy::StrictOriginWhenCrossOrigin => Self::StrictOriginWhenCrossOrigin,
      ReferrerPolicy::UnsafeUrl => Self::UnsafeUrl,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetworkFetchContextKind {
  Document,
  Iframe,
  Stylesheet,
  StylesheetCors,
  Script,
  ScriptCors,
  Image,
  ImageCors,
  Font,
  Other,
}

impl From<NetworkFetchContextKind> for FetchContextKind {
  fn from(value: NetworkFetchContextKind) -> Self {
    match value {
      NetworkFetchContextKind::Document => FetchContextKind::Document,
      NetworkFetchContextKind::Iframe => FetchContextKind::Iframe,
      NetworkFetchContextKind::Stylesheet => FetchContextKind::Stylesheet,
      NetworkFetchContextKind::StylesheetCors => FetchContextKind::StylesheetCors,
      NetworkFetchContextKind::Script => FetchContextKind::Script,
      NetworkFetchContextKind::ScriptCors => FetchContextKind::ScriptCors,
      NetworkFetchContextKind::Image => FetchContextKind::Image,
      NetworkFetchContextKind::ImageCors => FetchContextKind::ImageCors,
      NetworkFetchContextKind::Font => FetchContextKind::Font,
      NetworkFetchContextKind::Other => FetchContextKind::Other,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetworkRequestRedirect {
  Follow,
  Error,
  Manual,
}

impl From<NetworkRequestRedirect> for resource::web_fetch::RequestRedirect {
  fn from(value: NetworkRequestRedirect) -> Self {
    match value {
      NetworkRequestRedirect::Follow => resource::web_fetch::RequestRedirect::Follow,
      NetworkRequestRedirect::Error => resource::web_fetch::RequestRedirect::Error,
      NetworkRequestRedirect::Manual => resource::web_fetch::RequestRedirect::Manual,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkRequest {
  Fetch { url: String },
  FetchWithRequest { req: NetworkFetchRequest },
  FetchWithRequestAndValidation {
    req: NetworkFetchRequest,
    etag: Option<String>,
    last_modified: Option<String>,
  },
  FetchHttpRequest { req: NetworkHttpRequest },
  FetchPartialWithContext {
    kind: NetworkFetchContextKind,
    url: String,
    max_bytes: usize,
  },
  RequestHeaderValue {
    req: NetworkFetchRequest,
    header_name: String,
  },
  CookieHeaderValue { url: String },
  StoreCookieFromDocument { url: String, cookie_string: String },
  Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkResponse {
  Fetch(NetworkResult<NetworkFetchedResource>),
  RequestHeaderValue(NetworkResult<Option<String>>),
  CookieHeaderValue(NetworkResult<Option<String>>),
  StoreCookieFromDocument(NetworkResult<()>),
  ShutdownAck,
}

struct NetworkCall {
  request: NetworkRequest,
  respond_to: mpsc::Sender<NetworkResponse>,
}

#[derive(Clone)]
pub struct NetworkClient {
  tx: mpsc::Sender<NetworkCall>,
}

impl NetworkClient {
  fn roundtrip(&self, request: NetworkRequest) -> NetworkResult<NetworkResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    self
      .tx
      .send(NetworkCall {
        request,
        respond_to: reply_tx,
      })
      .map_err(|_| NetworkError::service_disconnected())?;
    reply_rx.recv().map_err(|_| NetworkError::service_disconnected())
  }

  pub fn fetch(&self, url: impl Into<String>) -> NetworkResult<NetworkFetchedResource> {
    let url = url.into();
    match self.roundtrip(NetworkRequest::Fetch { url: url.clone() })? {
      NetworkResponse::Fetch(result) => result,
      _ => Err(NetworkError {
        url,
        message: "unexpected network response".to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      }),
    }
  }

  pub fn fetch_with_request(&self, req: NetworkFetchRequest) -> NetworkResult<NetworkFetchedResource> {
    let url = req.url.clone();
    match self.roundtrip(NetworkRequest::FetchWithRequest { req })? {
      NetworkResponse::Fetch(result) => result,
      _ => Err(NetworkError {
        url,
        message: "unexpected network response".to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      }),
    }
  }

  pub fn fetch_with_request_and_validation(
    &self,
    req: NetworkFetchRequest,
    etag: Option<String>,
    last_modified: Option<String>,
  ) -> NetworkResult<NetworkFetchedResource> {
    let url = req.url.clone();
    match self.roundtrip(NetworkRequest::FetchWithRequestAndValidation {
      req,
      etag,
      last_modified,
    })? {
      NetworkResponse::Fetch(result) => result,
      _ => Err(NetworkError {
        url,
        message: "unexpected network response".to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      }),
    }
  }

  pub fn fetch_http_request(&self, req: NetworkHttpRequest) -> NetworkResult<NetworkFetchedResource> {
    let url = req.fetch.url.clone();
    match self.roundtrip(NetworkRequest::FetchHttpRequest { req })? {
      NetworkResponse::Fetch(result) => result,
      _ => Err(NetworkError {
        url,
        message: "unexpected network response".to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      }),
    }
  }

  pub fn fetch_partial_with_context(
    &self,
    kind: NetworkFetchContextKind,
    url: impl Into<String>,
    max_bytes: usize,
  ) -> NetworkResult<NetworkFetchedResource> {
    let url = url.into();
    match self.roundtrip(NetworkRequest::FetchPartialWithContext {
      kind,
      url: url.clone(),
      max_bytes,
    })? {
      NetworkResponse::Fetch(result) => result,
      _ => Err(NetworkError {
        url,
        message: "unexpected network response".to_string(),
        status: None,
        final_url: None,
        content_type: None,
        etag: None,
        last_modified: None,
      }),
    }
  }

  pub fn request_header_value(
    &self,
    req: NetworkFetchRequest,
    header_name: impl Into<String>,
  ) -> NetworkResult<Option<String>> {
    match self.roundtrip(NetworkRequest::RequestHeaderValue {
      req,
      header_name: header_name.into(),
    })? {
      NetworkResponse::RequestHeaderValue(result) => result,
      _ => Err(NetworkError::service_disconnected()),
    }
  }

  pub fn cookie_header_value(&self, url: impl Into<String>) -> NetworkResult<Option<String>> {
    let url = url.into();
    match self.roundtrip(NetworkRequest::CookieHeaderValue { url })? {
      NetworkResponse::CookieHeaderValue(result) => result,
      _ => Err(NetworkError::service_disconnected()),
    }
  }

  pub fn store_cookie_from_document(
    &self,
    url: impl Into<String>,
    cookie_string: impl Into<String>,
  ) -> NetworkResult<()> {
    let url = url.into();
    match self.roundtrip(NetworkRequest::StoreCookieFromDocument {
      url,
      cookie_string: cookie_string.into(),
    })? {
      NetworkResponse::StoreCookieFromDocument(result) => result,
      _ => Err(NetworkError::service_disconnected()),
    }
  }
}

pub struct NetworkService {
  tx: mpsc::Sender<NetworkCall>,
  join: Option<thread::JoinHandle<()>>,
}

impl NetworkService {
  pub fn start(fetcher: HttpFetcher) -> Self {
    let (tx, rx) = mpsc::channel::<NetworkCall>();
    let fetcher = Arc::new(fetcher);
    let join = thread::spawn(move || run_network_service(fetcher, rx));
    Self {
      tx,
      join: Some(join),
    }
  }

  pub fn start_default() -> Self {
    Self::start(HttpFetcher::new().with_timeout(Duration::from_secs(5)))
  }

  pub fn client(&self) -> NetworkClient {
    NetworkClient { tx: self.tx.clone() }
  }

  pub fn shutdown(mut self) {
    self.send_shutdown();
    if let Some(handle) = self.join.take() {
      let _ = handle.join();
    }
  }

  fn send_shutdown(&self) {
    let (reply_tx, _reply_rx) = mpsc::channel();
    let _ = self.tx.send(NetworkCall {
      request: NetworkRequest::Shutdown,
      respond_to: reply_tx,
    });
  }
}

impl Drop for NetworkService {
  fn drop(&mut self) {
    self.send_shutdown();
    if let Some(handle) = self.join.take() {
      let _ = handle.join();
    }
  }
}

fn run_network_service(fetcher: Arc<HttpFetcher>, rx: mpsc::Receiver<NetworkCall>) {
  while let Ok(call) = rx.recv() {
    let response = match call.request {
      NetworkRequest::Fetch { url } => {
        let result = fetcher.fetch(&url).map(NetworkFetchedResource::from).map_err(|err| {
          NetworkError::from_render_error(&url, err)
        });
        NetworkResponse::Fetch(result)
      }
      NetworkRequest::FetchWithRequest { req } => {
        let url = req.url.clone();
        let result = req.with_fetch_request(|fetch_req| fetcher.fetch_with_request(fetch_req))
          .map(NetworkFetchedResource::from)
          .map_err(|err| NetworkError::from_render_error(&url, err));
        NetworkResponse::Fetch(result)
      }
      NetworkRequest::FetchWithRequestAndValidation {
        req,
        etag,
        last_modified,
      } => {
        let url = req.url.clone();
        let result = req.with_fetch_request(|fetch_req| {
          fetcher.fetch_with_request_and_validation(fetch_req, etag.as_deref(), last_modified.as_deref())
        })
        .map(NetworkFetchedResource::from)
        .map_err(|err| NetworkError::from_render_error(&url, err));
        NetworkResponse::Fetch(result)
      }
      NetworkRequest::FetchHttpRequest { req } => {
        let url = req.fetch.url.clone();
        let NetworkHttpRequest {
          fetch,
          method,
          redirect,
          headers,
          body,
        } = req;
        let result = fetch.with_fetch_request(|fetch_req| {
          let request = HttpRequest {
            fetch: fetch_req,
            method: &method,
            redirect: redirect.into(),
            headers: &headers,
            body: body.as_deref(),
          };
          fetcher.fetch_http_request(request)
        })
        .map(NetworkFetchedResource::from)
        .map_err(|err| NetworkError::from_render_error(&url, err));
        NetworkResponse::Fetch(result)
      }
      NetworkRequest::FetchPartialWithContext {
        kind,
        url,
        max_bytes,
      } => {
        let result = fetcher
          .fetch_partial_with_context(kind.into(), &url, max_bytes)
          .map(NetworkFetchedResource::from)
          .map_err(|err| NetworkError::from_render_error(&url, err));
        NetworkResponse::Fetch(result)
      }
      NetworkRequest::RequestHeaderValue { req, header_name } => {
        let value = req.with_fetch_request(|fetch_req| fetcher.request_header_value(fetch_req, &header_name));
        NetworkResponse::RequestHeaderValue(Ok(value))
      }
      NetworkRequest::CookieHeaderValue { url } => {
        let value = fetcher.cookie_header_value(&url);
        NetworkResponse::CookieHeaderValue(Ok(value))
      }
      NetworkRequest::StoreCookieFromDocument { url, cookie_string } => {
        fetcher.store_cookie_from_document(&url, &cookie_string);
        NetworkResponse::StoreCookieFromDocument(Ok(()))
      }
      NetworkRequest::Shutdown => {
        let _ = call.respond_to.send(NetworkResponse::ShutdownAck);
        break;
      }
    };
    let _ = call.respond_to.send(response);
  }
}

  #[cfg(test)]
  mod tests {
    use super::*;
    use crate::testing::{net_test_lock, try_bind_localhost};
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;
  use url::Url;

  fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
      let n = stream.read(&mut tmp)?;
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&tmp[..n]);
      if buf.windows(4).any(|w| w == b"\r\n\r\n") {
        break;
      }
      if buf.len() > 64 * 1024 {
        break;
      }
    }
    Ok(buf)
  }

  fn path_from_request(request: &[u8]) -> String {
    let request = String::from_utf8_lossy(request);
    let request_line = request.lines().next().unwrap_or("").trim_end_matches('\r');
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    Url::parse(target)
      .ok()
      .map(|url| url.path().to_string())
      .unwrap_or_else(|| target.to_string())
  }

  fn cookie_from_request(request: &[u8]) -> String {
    let request = String::from_utf8_lossy(request);
    for line in request.lines() {
      let line = line.trim_end_matches('\r');
      if let Some(rest) = line.strip_prefix("Cookie:") {
        return rest.trim().to_string();
      }
      if let Some(rest) = line.strip_prefix("cookie:") {
        return rest.trim().to_string();
      }
    }
    String::new()
  }

  fn has_header(headers: &[(String, String)], needle: &str, expected: &str) -> bool {
    headers.iter().any(|(name, value)| {
      name.eq_ignore_ascii_case(needle) && value.trim() == expected
    })
  }

  #[test]
  fn network_service_fetch_with_request_roundtrip_and_cookies() {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("network_service_fetch_with_request_roundtrip_and_cookies") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      for _ in 0..2 {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let request = read_http_request(&mut stream).unwrap();
        let path = path_from_request(&request);
        match path.as_str() {
          "/set" => {
            let body = b"hello";
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nX-Test: 1\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          "/check" => {
            let cookie = cookie_from_request(&request);
            let body = cookie.as_bytes();
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => {
            let headers = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(headers.as_bytes());
          }
        }
      }
    });

    let service = NetworkService::start(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let client_a = service.client();
    let client_b = service.client();

    let url_set = format!("http://{addr}/set");
    let req = NetworkFetchRequest::new(url_set, NetworkFetchDestination::Document);
    let res = client_a.fetch_with_request(req).expect("fetch /set");
    assert_eq!(res.bytes, b"hello");
    assert_eq!(res.status, Some(200));
    let headers = res.response_headers.as_ref().expect("expected response headers");
    assert!(
      has_header(headers, "X-Test", "1"),
      "expected X-Test header in response headers, got {headers:?}"
    );

    let url_check = format!("http://{addr}/check");
    let req = NetworkFetchRequest::new(url_check, NetworkFetchDestination::Document);
    let res = client_b.fetch_with_request(req).expect("fetch /check");
    let cookie = String::from_utf8_lossy(&res.bytes);
    assert!(
      cookie.contains("a=b"),
      "expected cookie a=b to be sent on subsequent request, got body {cookie:?}"
    );

    handle.join().unwrap();
    drop(service);
  }
}
