use crate::debug::runtime;
use crate::error::{Error, Result};
use crate::resource::{
  ensure_cors_allows_origin, CorsMode, DocumentOrigin, FetchCredentialsMode, FetchDestination,
  FetchRequest, HttpRequest, ResourceFetcher,
};

use super::{
  Body, Headers, HeadersGuard, Request, RequestCredentials, RequestMode, Response, ResponseType,
};

#[derive(Debug, Clone, Copy)]
pub struct WebFetchExecutionContext<'a> {
  pub destination: FetchDestination,
  pub referrer_url: Option<&'a str>,
  pub client_origin: Option<&'a DocumentOrigin>,
}

impl<'a> Default for WebFetchExecutionContext<'a> {
  fn default() -> Self {
    Self {
      destination: FetchDestination::Fetch,
      referrer_url: None,
      client_origin: None,
    }
  }
}

pub fn execute_web_fetch(
  fetcher: &dyn ResourceFetcher,
  request: &Request,
  ctx: WebFetchExecutionContext<'_>,
) -> Result<Response> {
  let method = request.method.as_str();
  let is_get_or_head = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
  if is_get_or_head && request.body.is_some() {
    return Err(Error::Other(format!(
      "web fetch request body is not allowed for {method}"
    )));
  }

  let requested_url = request.url.as_str();
  let credentials_mode = FetchCredentialsMode::from(request.credentials);
  let referrer_policy = request.referrer_policy;
  let user_headers = request.headers.raw_pairs();
  let body_bytes = request.body.as_ref().map(|body| body.as_bytes());
  let fetch_request = FetchRequest {
    url: requested_url,
    destination: ctx.destination,
    referrer_url: ctx.referrer_url,
    client_origin: ctx.client_origin,
    referrer_policy,
    credentials_mode,
  };

  let mut resource = fetcher.fetch_http_request(HttpRequest {
    fetch: fetch_request,
    method,
    headers: user_headers.as_slice(),
    body: body_bytes,
  })?;

  if request.mode == RequestMode::Cors {
    // Unlike subresource CORS enforcement (gated by `FASTR_FETCH_ENFORCE_CORS`), Fetch API
    // `mode: "cors"` requests always run CORS validation.
    let cors_mode = match request.credentials {
      RequestCredentials::Include => CorsMode::UseCredentials,
      RequestCredentials::Omit | RequestCredentials::SameOrigin => CorsMode::Anonymous,
    };
    ensure_cors_allows_origin(ctx.client_origin, &resource, requested_url, cors_mode)?;
  }

  if method.eq_ignore_ascii_case("HEAD") {
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
    r#type: ResponseType::Default,
    url,
    redirected,
    status,
    status_text: String::new(),
    headers,
    body,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::web_fetch::WebFetchError;
  use crate::resource::{origin_from_url, FetchedResource, ReferrerPolicy};

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

    fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
      panic!("fetch_http_request should not be called")
    }
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
      resource: FetchedResource::new(br#"{\"ok\": true}"#.to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let mut response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    let body = response.body.as_mut().expect("expected body");
    let value = body.json().unwrap();
    assert_eq!(value, serde_json::json!({"ok": true}));
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
    assert!(err
      .to_string()
      .contains("Access-Control-Allow-Origin * is not allowed"));
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
  fn forwards_request_fields_and_execution_context_to_http_request() {
    struct HttpRequestAssertingFetcher {
      expected_method: &'static str,
      expected_headers: Vec<(String, String)>,
      expected_body: Option<Vec<u8>>,
      expected_destination: FetchDestination,
      expected_referrer_url: Option<&'static str>,
      expected_client_origin: DocumentOrigin,
      expected_referrer_policy: ReferrerPolicy,
      expected_credentials_mode: FetchCredentialsMode,
      resource: FetchedResource,
    }

    impl ResourceFetcher for HttpRequestAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request")
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, self.expected_method);
        assert_eq!(req.headers, self.expected_headers.as_slice());
        assert_eq!(req.body, self.expected_body.as_deref());

        assert_eq!(req.fetch.destination, self.expected_destination);
        assert_eq!(req.fetch.referrer_url, self.expected_referrer_url);
        assert!(req
          .fetch
          .client_origin
          .is_some_and(|origin| origin == &self.expected_client_origin));
        assert_eq!(req.fetch.referrer_policy, self.expected_referrer_policy);
        assert_eq!(req.fetch.credentials_mode, self.expected_credentials_mode);

        Ok(self.resource.clone())
      }
    }

    let origin = origin_from_url("https://example.com/").expect("origin");
    let fetcher = HttpRequestAssertingFetcher {
      expected_method: "POST",
      expected_headers: vec![
        ("x-test".to_string(), "a".to_string()),
        ("x-test".to_string(), "b".to_string()),
        ("x-other".to_string(), "c".to_string()),
      ],
      expected_body: Some(b"hello".to_vec()),
      expected_destination: FetchDestination::Fetch,
      expected_referrer_url: Some("https://referrer.example/page"),
      expected_client_origin: origin.clone(),
      expected_referrer_policy: ReferrerPolicy::NoReferrer,
      expected_credentials_mode: FetchCredentialsMode::Include,
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };

    let mut request = Request::new("POST", "https://example.com/a");
    request.headers.append("X-Test", "a").unwrap();
    request.headers.append("x-test", "b").unwrap();
    request.headers.append("X-Other", "c").unwrap();
    request.credentials = RequestCredentials::Include;
    request.referrer_policy = ReferrerPolicy::NoReferrer;
    request.body = Some(Body::new(b"hello".to_vec()));

    let ctx = WebFetchExecutionContext {
      destination: FetchDestination::Fetch,
      referrer_url: Some("https://referrer.example/page"),
      client_origin: Some(&origin),
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn head_response_body_is_empty() {
    struct HeadBytesFetcher;

    impl ResourceFetcher for HeadBytesFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request")
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
}
