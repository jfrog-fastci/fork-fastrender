use crate::debug::runtime;
use crate::error::{Error, Result};
use crate::resource::{
  ensure_cors_allows_origin, origin_from_url, CorsMode, DocumentOrigin, FetchCredentialsMode,
  FetchDestination, FetchRequest, HttpRequest, ReferrerPolicy, ResourceFetcher,
};

use super::{
  Body, Headers, HeadersGuard, Request, RequestCredentials, RequestMode, Response, ResponseType,
};

#[derive(Debug, Clone, Copy)]
pub struct WebFetchExecutionContext<'a> {
  pub referrer_url: Option<&'a str>,
  pub client_origin: Option<&'a DocumentOrigin>,
  pub referrer_policy: crate::resource::ReferrerPolicy,
  pub credentials_mode_override: Option<FetchCredentialsMode>,
}

impl<'a> Default for WebFetchExecutionContext<'a> {
  fn default() -> Self {
    Self {
      referrer_url: None,
      client_origin: None,
      referrer_policy: crate::resource::ReferrerPolicy::default(),
      credentials_mode_override: None,
    }
  }
}

pub fn execute_web_fetch(
  fetcher: &dyn ResourceFetcher,
  request: &Request,
  ctx: WebFetchExecutionContext<'_>,
) -> Result<Response> {
  let method = request.method.as_str();
  let method_is_get = method.eq_ignore_ascii_case("GET");
  let method_is_head = method.eq_ignore_ascii_case("HEAD");
  let method_is_post = method.eq_ignore_ascii_case("POST");
  if !(method_is_get || method_is_head || method_is_post) {
    return Err(Error::Other(format!(
      "web fetch currently supports only GET/HEAD/POST (got method {method:?})"
    )));
  }

  if (method_is_get || method_is_head) && request.body.is_some() {
    return Err(Error::Other(
      "web fetch request body is not allowed for GET/HEAD".to_string(),
    ));
  }

  let requested_url = request.url.as_str();
  if request.mode == RequestMode::SameOrigin {
    let Some(client_origin) = ctx.client_origin else {
      return Err(Error::Other(
        "web fetch same-origin request requires a client origin".to_string(),
      ));
    };
    let Some(target_origin) = origin_from_url(requested_url) else {
      return Err(Error::Other(format!(
        "web fetch same-origin request requires a valid URL (got {requested_url:?})"
      )));
    };
    if !client_origin.same_origin(&target_origin) {
      return Err(Error::Other(format!(
        "web fetch blocked cross-origin URL for same-origin mode (client origin {client_origin}, target origin {target_origin})"
      )));
    }
  }

  let referrer_url = if request.referrer.trim().is_empty() {
    ctx.referrer_url
  } else {
    Some(request.referrer.as_str())
  };
  let referrer_policy = if request.referrer_policy != ReferrerPolicy::EmptyString {
    request.referrer_policy
  } else {
    ctx.referrer_policy
  };
  let credentials_mode = ctx
    .credentials_mode_override
    .unwrap_or_else(|| request.credentials.into());

  let destination = FetchDestination::Fetch;
  let fetch_request = FetchRequest {
    url: requested_url,
    destination,
    referrer_url,
    client_origin: ctx.client_origin,
    referrer_policy,
    credentials_mode,
  };

  let mut request_headers = Headers::new_with_guard(match request.mode {
    RequestMode::NoCors => HeadersGuard::RequestNoCors,
    _ => HeadersGuard::Request,
  });
  // Re-apply the appropriate guard for the current request mode so callers don't have to keep the
  // `Headers` guard in sync when they mutate `Request.mode` directly.
  request_headers
    .fill_from_pairs(request.headers.sort_and_combine())
    .map_err(|err| Error::Other(err.to_string()))?;
  let user_header_pairs = request_headers.sort_and_combine();

  let body_bytes = request.body.as_ref().map(|body| body.as_bytes());

  let http_req = HttpRequest {
    fetch: fetch_request,
    method,
    redirect: request.redirect,
    headers: &user_header_pairs,
    body: body_bytes,
  };

  let mut resource = fetcher.fetch_http_request(http_req)?;

  if request.mode == RequestMode::Cors {
    // Unlike subresource CORS enforcement (gated by `FASTR_FETCH_ENFORCE_CORS`), Fetch API
    // `mode: "cors"` requests always run CORS validation.
    let cors_mode = match request.credentials {
      RequestCredentials::Include => CorsMode::UseCredentials,
      RequestCredentials::Omit | RequestCredentials::SameOrigin => CorsMode::Anonymous,
    };
    ensure_cors_allows_origin(ctx.client_origin, &resource, requested_url, cors_mode)?;
  }

  if method_is_head {
    resource.bytes.clear();
  }

  let status = resource.status.unwrap_or(200);
  let url = resource
    .final_url
    .take()
    .unwrap_or_else(|| requested_url.to_string());
  let redirected = url != requested_url;

  let mut headers = Headers::new_with_guard(HeadersGuard::Response);
  if let Some(response_headers) = resource.response_headers.take() {
    for (name, value) in response_headers {
      if let Err(err) = headers.append(&name, &value) {
        if runtime::runtime_toggles().truthy("FASTR_WEB_FETCH_DEBUG") {
          eprintln!(
            "web fetch: skipping invalid response header {name:?}: {value:?} ({err})"
          );
        }
      }
    }
  }

  let body = Some(Body::new(std::mem::take(&mut resource.bytes)));

  Ok(Response {
    r#type: match request.mode {
      RequestMode::NoCors => ResponseType::Opaque,
      RequestMode::Cors => ResponseType::Cors,
      RequestMode::SameOrigin => ResponseType::Basic,
      RequestMode::Navigate => ResponseType::Default,
    },
    url,
    redirected,
    status,
    status_text: http::StatusCode::from_u16(status)
      .ok()
      .and_then(|code| code.canonical_reason())
      .unwrap_or("")
      .to_string(),
    headers,
    body,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::web_fetch::WebFetchError;
  use crate::resource::{origin_from_url, FetchedResource, HttpFetcher, HttpRetryPolicy};
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::sync::{Arc, Mutex};
  use std::thread;
  use std::time::Duration;

  struct StaticFetcher {
    resource: FetchedResource,
  }

  impl ResourceFetcher for StaticFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      Ok(self.resource.clone())
    }
  }

  struct PanicFetcher;

  impl ResourceFetcher for PanicFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("fetch should not be called")
    }

    fn fetch_with_request(&self, _req: FetchRequest<'_>) -> Result<FetchedResource> {
      panic!("fetch_with_request should not be called")
    }
  }

  fn curl_binary_available() -> bool {
    std::process::Command::new("curl")
      .arg("--version")
      .output()
      .is_ok()
  }

  fn skip_if_curl_backend_missing(test_name: &str) -> bool {
    let backend = std::env::var("FASTR_HTTP_BACKEND")
      .ok()
      .unwrap_or_default()
      .trim()
      .to_ascii_lowercase();
    if backend == "curl" && !curl_binary_available() {
      eprintln!("skipping {test_name}: curl backend selected but curl is unavailable");
      true
    } else {
      false
    }
  }

  fn try_bind_localhost(test_name: &str) -> Option<TcpListener> {
    match TcpListener::bind(("127.0.0.1", 0)) {
      Ok(listener) => Some(listener),
      Err(err) => {
        eprintln!("skipping {test_name}: failed to bind localhost socket: {err}");
        None
      }
    }
  }

  fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
      .windows(needle.len())
      .position(|window| window == needle)
  }

  fn decode_chunked_body(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut remaining = raw;
    loop {
      let line_end = find_subsequence(remaining, b"\r\n")?;
      let line = std::str::from_utf8(&remaining[..line_end]).ok()?;
      let size_str = line.split(';').next().unwrap_or("");
      let size = usize::from_str_radix(size_str.trim(), 16).ok()?;
      remaining = &remaining[line_end + 2..];
      if size == 0 {
        return Some(out);
      }
      if remaining.len() < size + 2 {
        return None;
      }
      out.extend_from_slice(&remaining[..size]);
      remaining = &remaining[size + 2..];
    }
  }

  fn read_http_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
      let read = stream.read(&mut tmp).expect("read request");
      if read == 0 {
        panic!("unexpected EOF while reading request headers");
      }
      buf.extend_from_slice(&tmp[..read]);
      if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
        header_end = pos + 4;
        break;
      }
      assert!(buf.len() < 1024 * 64, "request headers too large");
    }

    let header_bytes = &buf[..header_end];
    let header_str = String::from_utf8_lossy(header_bytes).to_string();
    let header_lower = header_str.to_ascii_lowercase();

    let mut body = buf[header_end..].to_vec();

    if let Some(len_line) = header_lower
      .lines()
      .find(|line| line.starts_with("content-length:"))
    {
      let len = len_line["content-length:".len()..].trim().parse::<usize>().unwrap();
      while body.len() < len {
        let read = stream.read(&mut tmp).expect("read request body");
        if read == 0 {
          break;
        }
        body.extend_from_slice(&tmp[..read]);
      }
      body.truncate(len);
      return (header_str, body);
    }

    if header_lower.contains("transfer-encoding: chunked") {
      loop {
        if let Some(decoded) = decode_chunked_body(&body) {
          return (header_str, decoded);
        }
        let read = stream.read(&mut tmp).expect("read chunked body");
        if read == 0 {
          break;
        }
        body.extend_from_slice(&tmp[..read]);
      }
      panic!("incomplete chunked body");
    }

    (header_str, body)
  }

  fn test_http_fetcher() -> HttpFetcher {
    HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        backoff_base: Duration::ZERO,
        backoff_cap: Duration::ZERO,
        respect_retry_after: true,
      })
  }

  #[test]
  fn response_sets_status_url_redirected() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"hello".to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(response.url, "https://example.com/a");
    assert!(!response.redirected);

    let mut resource = FetchedResource::new(b"missing".to_vec(), None);
    resource.status = Some(404);
    resource.final_url = Some("https://example.com/b".to_string());
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.status, 404);
    assert_eq!(response.url, "https://example.com/b");
    assert!(response.redirected);
  }

  #[test]
  fn response_headers_populate_and_respect_response_guard() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.response_headers = Some(vec![
      ("Content-Type".to_string(), "text/plain".to_string()),
      ("X-Test".to_string(), "hello".to_string()),
      ("Set-Cookie".to_string(), "a=b".to_string()),
    ]);
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    assert_eq!(response.headers.guard(), HeadersGuard::Response);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert_eq!(
      response.headers.get("x-test").unwrap().as_deref(),
      Some("hello")
    );
    assert!(!response.headers.has("set-cookie").unwrap());
  }

  #[test]
  fn response_body_text_utf8_marks_body_used() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"hello".to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let mut response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    let body = response.body.as_mut().expect("expected body");
    assert_eq!(body.text_utf8().unwrap(), "hello");
    assert!(body.body_used());

    let err = body.consume_bytes().unwrap_err();
    assert!(matches!(err, WebFetchError::BodyUsed));
  }

  #[test]
  fn response_body_json() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(br#"{"ok": true}"#.to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let mut response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    let body = response.body.as_mut().expect("expected body");
    let value = body.json().unwrap();
    assert_eq!(value, serde_json::json!({"ok": true}));
  }

  #[test]
  fn unsupported_method_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let request = Request::new("PUT", "https://example.com/a");
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn request_body_on_get_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "https://example.com/a");
    request.body = Some(Body::new(b"x".to_vec()));
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn request_body_on_head_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("HEAD", "https://example.com/a");
    request.body = Some(Body::new(b"x".to_vec()));
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn skips_invalid_response_headers() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.response_headers = Some(vec![
      ("bad header".to_string(), "x".to_string()),
      ("x-ok".to_string(), "y".to_string()),
    ]);
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.headers.get("x-ok").unwrap().as_deref(), Some("y"));
  }

  #[test]
  fn cors_enforcement_blocks_mismatched_origin() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };
    let request = Request::new("GET", "https://other.example/res");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected CORS error");
    assert!(matches!(err, Error::Resource(_)));
    assert!(err.to_string().contains("blocked by CORS"));
  }

  #[test]
  fn cors_allows_matching_origin() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    let fetcher = StaticFetcher { resource };

    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected CORS pass");
  }

  #[test]
  fn cors_allows_wildcard_for_anonymous_but_not_credentialed() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("*".to_string());
    let fetcher = StaticFetcher {
      resource: resource.clone(),
    };

    let origin = origin_from_url("https://client.example/").expect("origin");
    let mut request = Request::new("GET", "https://other.example/res");

    request.credentials = RequestCredentials::Omit;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    execute_web_fetch(&fetcher, &request, ctx).expect("expected wildcard CORS pass");

    request.credentials = RequestCredentials::SameOrigin;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    execute_web_fetch(&fetcher, &request, ctx).expect("expected wildcard CORS pass");

    let fetcher = StaticFetcher { resource };
    request.credentials = RequestCredentials::Include;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected wildcard CORS fail");
    assert!(err.to_string().contains("Access-Control-Allow-Origin * is not allowed"));
  }

  #[test]
  fn cors_rejects_comma_separated_allow_origin() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin =
      Some("https://client.example, https://other.example".to_string());
    let fetcher = StaticFetcher { resource };

    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let err =
      execute_web_fetch(&fetcher, &request, ctx).expect_err("expected comma-separated CORS fail");
    assert!(err.to_string().contains("multiple values"));
  }

  #[test]
  fn cors_credentialed_requests_require_allow_credentials() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    resource.access_control_allow_credentials = false;
    let fetcher = StaticFetcher { resource };

    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Include;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let err = execute_web_fetch(&fetcher, &request, ctx)
      .expect_err("expected credentialed CORS to require allow-credentials");
    assert!(err
      .to_string()
      .contains("missing Access-Control-Allow-Credentials: true"));
  }

  #[test]
  fn forwards_execution_context_to_fetch_request() {
    struct ContextAssertingFetcher {
      expected_referrer_url: &'static str,
      expected_referrer_policy: crate::resource::ReferrerPolicy,
      expected_credentials_mode: FetchCredentialsMode,
    }

    impl ResourceFetcher for ContextAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.fetch.destination, FetchDestination::Fetch);
        assert_eq!(req.fetch.referrer_url, Some(self.expected_referrer_url));
        assert_eq!(req.fetch.referrer_policy, self.expected_referrer_policy);
        assert_eq!(req.fetch.credentials_mode, self.expected_credentials_mode);
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }
    }

    let origin = origin_from_url("https://example.com/").expect("origin");
    let fetcher = ContextAssertingFetcher {
      expected_referrer_url: "https://referrer.example/page",
      expected_referrer_policy: crate::resource::ReferrerPolicy::NoReferrer,
      expected_credentials_mode: FetchCredentialsMode::Include,
    };

    let mut request = Request::new("GET", "https://example.com/a");
    request.referrer = "https://referrer.example/page".to_string();
    request.referrer_policy = crate::resource::ReferrerPolicy::NoReferrer;

    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://ignored.example/"),
      client_origin: Some(&origin),
      referrer_policy: crate::resource::ReferrerPolicy::StrictOriginWhenCrossOrigin,
      credentials_mode_override: Some(FetchCredentialsMode::Include),
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn head_response_body_is_empty() {
    struct HeadBytesFetcher;

    impl ResourceFetcher for HeadBytesFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, "HEAD");
        assert!(req.headers.is_empty());
        assert!(req.body.is_none());
        Ok(FetchedResource::new(b"should-be-ignored".to_vec(), None))
      }
    }

    let fetcher = HeadBytesFetcher;
    let request = Request::new("HEAD", "https://example.com/a");
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
        .expect("expected response");

    let body = response.body.as_mut().expect("expected body");
    assert_eq!(body.consume_bytes().unwrap(), Vec::<u8>::new());
  }

  #[test]
  fn sends_user_headers_over_network() {
    if skip_if_curl_backend_missing("sends_user_headers_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("sends_user_headers_over_network") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      *captured_req.lock().unwrap() = headers;
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/headers");
    let mut request = Request::new("GET", &url);
    request.headers.append("X-Test", "hello").unwrap();
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();

    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(req.contains("x-test: hello"), "expected header, got:\n{req}");
  }

  #[test]
  fn sends_post_body_over_network() {
    if skip_if_curl_backend_missing("sends_post_body_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("sends_post_body_over_network") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (_headers, body) = read_http_request(&mut stream);
      *captured_req.lock().unwrap() = body;
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/submit");
    let mut request = Request::new("POST", &url);
    request.body = Some(Body::new(b"hello".to_vec()));
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();
    assert_eq!(&*captured.lock().unwrap(), b"hello");
  }

  #[test]
  fn redirect_follow_follows() {
    if skip_if_curl_backend_missing("redirect_follow_follows") {
      return;
    }
    let Some(listener) = try_bind_localhost("redirect_follow_follows") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // First request: redirect.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push(headers);
      let response =
        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Second request: final response.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push(headers);
      let body = b"done";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let request = Request::new("GET", &url);
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert!(response.redirected);
    assert!(response.url.ends_with("/final"), "unexpected url: {}", response.url);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"done");
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");
    let first = captured[0].to_ascii_lowercase();
    let second = captured[1].to_ascii_lowercase();
    assert!(first.starts_with("get /start"), "first request: {first}");
    assert!(second.starts_with("get /final"), "second request: {second}");
  }

  #[test]
  fn redirect_error_errors() {
    if skip_if_curl_backend_missing("redirect_error_errors") {
      return;
    }
    let Some(listener) = try_bind_localhost("redirect_error_errors") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let response =
        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure no follow-up request arrives.
      listener.set_nonblocking(true).unwrap();
      let start = std::time::Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected follow-up request in redirect=error mode"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after redirect: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let mut request = Request::new("GET", &url);
    request.redirect = crate::resource::web_fetch::RequestRedirect::Error;
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected redirect error");
    assert!(matches!(err, Error::Resource(_)));
    handle.join().unwrap();
  }

  #[test]
  fn redirect_manual_returns_302_without_following() {
    if skip_if_curl_backend_missing("redirect_manual_returns_302_without_following") {
      return;
    }
    let Some(listener) = try_bind_localhost("redirect_manual_returns_302_without_following") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let response =
        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure no follow-up request arrives.
      listener.set_nonblocking(true).unwrap();
      let start = std::time::Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected follow-up request in redirect=manual mode"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after redirect: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let mut request = Request::new("GET", &url);
    request.redirect = crate::resource::web_fetch::RequestRedirect::Manual;
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 302);
    assert!(!response.redirected);
    assert!(response.url.ends_with("/start"), "unexpected url: {}", response.url);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b""
    );
    handle.join().unwrap();
  }

  #[test]
  fn same_origin_blocks_cross_origin_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "https://other.example/res");
    request.mode = RequestMode::SameOrigin;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(
      err.to_string().contains("blocked cross-origin"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn no_cors_skips_cors_validation_and_returns_opaque() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };
    let mut request = Request::new("GET", "https://other.example/res");
    request.mode = RequestMode::NoCors;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Opaque);
  }
}
