use crate::debug::runtime;
use crate::error::{Error, ResourceError, Result};
use crate::html::content_security_policy::{CspDirective, CspPolicy};
use crate::resource::{
  origin_from_url, validate_cors_allow_origin, DocumentOrigin, FetchCredentialsMode,
  FetchDestination, FetchRequest, HttpRequest, ReferrerPolicy, ResourceFetcher,
};
use crate::url_normalize::{
  normalize_http_url_for_resolution, normalize_url_reference_for_resolution,
};
use http::{header::HeaderName, Method};
use std::collections::HashSet;
use url::Url;

use super::{
  Body, Headers, HeadersGuard, Request, RequestCredentials, RequestMode, RequestRedirect, Response,
  ResponseType, WebFetchError,
};

#[derive(Debug, Clone, Copy)]
pub struct WebFetchExecutionContext<'a> {
  pub destination: FetchDestination,
  pub referrer_url: Option<&'a str>,
  pub client_origin: Option<&'a DocumentOrigin>,
  pub referrer_policy: crate::resource::ReferrerPolicy,
  /// Optional CSP policy to enforce (`connect-src` / `default-src`) for Fetch API requests.
  pub csp: Option<&'a CspPolicy>,
}

impl<'a> Default for WebFetchExecutionContext<'a> {
  fn default() -> Self {
    Self {
      destination: FetchDestination::Fetch,
      referrer_url: None,
      client_origin: None,
      referrer_policy: crate::resource::ReferrerPolicy::default(),
      csp: None,
    }
  }
}

fn effective_referrer_url<'a>(
  request: &'a Request,
  ctx: WebFetchExecutionContext<'a>,
) -> Option<&'a str> {
  // The empty string represents "use the execution context's default referrer". Do not treat other
  // whitespace as empty: non-empty referrer strings must be honored verbatim so invalid inputs
  // don't silently fall back to the context referrer.
  if request.referrer.is_empty() {
    return ctx.referrer_url;
  }
  // `Request.referrer` is a URL string in the spec, but it can also carry the sentinel value
  // `"no-referrer"` to explicitly omit the referrer. FastRender uses that sentinel verbatim.
  if request.referrer == "no-referrer" {
    return None;
  }
  Some(request.referrer.as_str())
}

fn effective_referrer_policy(
  request: &Request,
  ctx: WebFetchExecutionContext<'_>,
) -> ReferrerPolicy {
  if request.referrer_policy != ReferrerPolicy::EmptyString {
    request.referrer_policy
  } else {
    ctx.referrer_policy
  }
}

fn origin_from_url_tolerant(url: &str) -> Option<DocumentOrigin> {
  origin_from_url(url).or_else(|| {
    let normalized = normalize_http_url_for_resolution(url);
    if normalized.as_ref() == url {
      return None;
    }
    origin_from_url(normalized.as_ref())
  })
}

fn url_base_for_origin(origin: &DocumentOrigin) -> Option<Url> {
  // `DocumentOrigin` stores only scheme/host/port (no path), so the best we can do is treat the
  // origin as the base directory.
  if origin.scheme() == "file" {
    return Url::parse("file:///").ok();
  }
  let host = origin.host()?;
  let host = if host.contains(':') && !host.starts_with('[') {
    format!("[{host}]")
  } else {
    host.to_string()
  };
  let scheme = origin.scheme();
  let url = match (scheme, origin.port()) {
    ("http", Some(80)) | ("https", Some(443)) => format!("{scheme}://{host}/"),
    (_, Some(port)) => format!("{scheme}://{host}:{port}/"),
    (_, None) => format!("{scheme}://{host}/"),
  };
  Url::parse(&url).ok()
}

fn is_cors_safelisted_response_header_name(name: &str) -> bool {
  // https://fetch.spec.whatwg.org/#cors-safelisted-response-header-name
  matches!(
    name,
    "cache-control"
      | "content-language"
      | "content-length"
      | "content-type"
      | "expires"
      | "last-modified"
      | "pragma"
  )
}

fn trim_http_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, ' ' | '\t'))
}

fn resolve_request_url<'a>(
  candidate: &'a str,
  base_url: Option<&str>,
  client_origin: Option<&DocumentOrigin>,
  storage: &'a mut Option<String>,
  max_url_bytes: usize,
) -> Option<&'a str> {
  if candidate.len() > max_url_bytes {
    return None;
  }
  let normalized_candidate = normalize_http_url_for_resolution(candidate);
  if let Ok(mut url) = Url::parse(candidate) {
    // Fetch strips URL fragments before issuing the network request.
    url.set_fragment(None);
    // Fetch uses a parsed URL record; serialize it back to a string so callers observe a canonical
    // URL (e.g. "https://example.com" → "https://example.com/") and so redirect detection doesn't
    // treat pure serialization differences as a redirect.
    let canonical = url.as_str();
    if canonical.len() > max_url_bytes {
      return None;
    }
    if canonical == candidate {
      return Some(candidate);
    }
    *storage = Some(canonical.to_string());
    return storage.as_deref();
  }
  if normalized_candidate.as_ref() != candidate {
    if normalized_candidate.len() > max_url_bytes {
      return None;
    }
    if let Ok(mut url) = Url::parse(normalized_candidate.as_ref()) {
      url.set_fragment(None);
      let canonical = url.as_str();
      if canonical.len() > max_url_bytes {
        return None;
      }
      *storage = Some(canonical.to_string());
      return storage.as_deref();
    }
  }

  let normalized_ref = normalize_url_reference_for_resolution(candidate);

  if let Some(raw_base) = base_url {
    if raw_base.len() > max_url_bytes {
      return None;
    }
    let normalized_base = normalize_http_url_for_resolution(raw_base);
    if normalized_base.len() > max_url_bytes {
      return None;
    }
    let base = Url::parse(normalized_base.as_ref())
      .or_else(|_| Url::parse(raw_base))
      .ok();

    if let Some(base) = base {
      if normalized_ref.as_ref() != candidate {
        if normalized_ref.len() > max_url_bytes {
          return None;
        }
        if let Ok(mut joined) = base.join(normalized_ref.as_ref()) {
          joined.set_fragment(None);
          let joined = joined.as_str();
          if joined.len() > max_url_bytes {
            return None;
          }
          *storage = Some(joined.to_string());
          return storage.as_deref();
        }
      }
      if let Ok(mut joined) = base.join(candidate) {
        joined.set_fragment(None);
        let joined = joined.as_str();
        if joined.len() > max_url_bytes {
          return None;
        }
        *storage = Some(joined.to_string());
        return storage.as_deref();
      }
    }
  }

  if let Some(origin) = client_origin.and_then(url_base_for_origin) {
    if normalized_ref.as_ref() != candidate {
      if normalized_ref.len() > max_url_bytes {
        return None;
      }
      if let Ok(mut joined) = origin.join(normalized_ref.as_ref()) {
        joined.set_fragment(None);
        let joined = joined.as_str();
        if joined.len() > max_url_bytes {
          return None;
        }
        *storage = Some(joined.to_string());
        return storage.as_deref();
      }
    }
    if let Ok(mut joined) = origin.join(candidate) {
      joined.set_fragment(None);
      let joined = joined.as_str();
      if joined.len() > max_url_bytes {
        return None;
      }
      *storage = Some(joined.to_string());
      return storage.as_deref();
    }
  }

  None
}

pub fn execute_web_fetch<'a>(
  fetcher: &dyn ResourceFetcher,
  request: &'a Request,
  ctx: WebFetchExecutionContext<'a>,
) -> Result<Response> {
  let max_url_bytes = request.headers.limits().max_url_bytes;
  let url_len = request.url.len();
  if url_len > max_url_bytes {
    return Err(Error::Other(format!(
      "web fetch request URL exceeds max_url_bytes (len={url_len}, limit={max_url_bytes})"
    )));
  }

  let raw_method = request.method.as_str();
  if Method::from_bytes(raw_method.as_bytes()).is_err() {
    return Err(Error::Other(format!(
      "web fetch request method is not a valid HTTP method token (got {:?})",
      request.method
    )));
  }

  // Fetch rejects forbidden methods (CONNECT/TRACE/TRACK).
  // https://fetch.spec.whatwg.org/#forbidden-method
  if raw_method.eq_ignore_ascii_case("CONNECT")
    || raw_method.eq_ignore_ascii_case("TRACE")
    || raw_method.eq_ignore_ascii_case("TRACK")
  {
    return Err(Error::Other(format!(
      "web fetch request method is forbidden (got {:?})",
      request.method
    )));
  }

  // Fetch normalizes a subset of method names to uppercase.
  // https://fetch.spec.whatwg.org/#concept-method-normalize
  let method = if raw_method.eq_ignore_ascii_case("DELETE") {
    "DELETE"
  } else if raw_method.eq_ignore_ascii_case("GET") {
    "GET"
  } else if raw_method.eq_ignore_ascii_case("HEAD") {
    "HEAD"
  } else if raw_method.eq_ignore_ascii_case("OPTIONS") {
    "OPTIONS"
  } else if raw_method.eq_ignore_ascii_case("POST") {
    "POST"
  } else if raw_method.eq_ignore_ascii_case("PUT") {
    "PUT"
  } else {
    raw_method
  };

  let method_is_get = method == "GET";
  let method_is_head = method == "HEAD";
  let method_is_post = method == "POST";

  // Fetch rejects `no-cors` requests whose redirect mode is not `follow`.
  // https://fetch.spec.whatwg.org/#scheme-fetch
  if request.mode == RequestMode::NoCors && request.redirect != RequestRedirect::Follow {
    return Err(Error::Other(format!(
      "web fetch no-cors requests require redirect mode \"follow\" (got {:?})",
      request.redirect
    )));
  }
  // Fetch also requires `no-cors` requests to use a CORS-safelisted method (GET/HEAD/POST).
  // https://fetch.spec.whatwg.org/#dom-request
  if request.mode == RequestMode::NoCors && !(method_is_get || method_is_head || method_is_post) {
    return Err(Error::Other(format!(
      "web fetch no-cors requests require a CORS-safelisted method (GET/HEAD/POST) (got {:?})",
      request.method
    )));
  }

  if (method_is_get || method_is_head) && request.body.is_some() {
    return Err(Error::Other(
      "web fetch request body is not allowed for GET/HEAD".to_string(),
    ));
  }

  let referrer_url = effective_referrer_url(request, ctx).filter(|u| u.len() <= max_url_bytes);
  let referrer_origin = ctx
    .client_origin
    .is_none()
    .then(|| referrer_url.and_then(origin_from_url_tolerant))
    .flatten();
  let client_origin = ctx.client_origin.or(referrer_origin.as_ref());

  let mut requested_url_storage: Option<String> = None;
  let requested_url = resolve_request_url(
    request.url.as_str(),
    referrer_url,
    client_origin,
    &mut requested_url_storage,
    max_url_bytes,
  )
  .ok_or_else(|| {
    Error::Other(format!(
      "web fetch request URL is invalid or missing base URL (got {:?})",
      request.url
    ))
  })?;

  if let Some(csp) = ctx.csp {
    let parsed = Url::parse(requested_url).map_err(|_| {
      Error::Other(format!(
        "Blocked by Content-Security-Policy ({}) for requested URL (invalid URL): {}",
        CspDirective::ConnectSrc.as_str(),
        requested_url
      ))
    })?;
    if !csp.allows_url(CspDirective::ConnectSrc, client_origin, &parsed) {
      return Err(Error::Other(format!(
        "Blocked by Content-Security-Policy ({}) for requested URL: {}",
        CspDirective::ConnectSrc.as_str(),
        parsed.as_str()
      )));
    }
  }
  if request.mode == RequestMode::SameOrigin {
    let Some(client_origin) = client_origin else {
      return Err(Error::Other(
        "web fetch same-origin mode requires a client origin; blocking request".to_string(),
      ));
    };
    let Some(target_origin) = origin_from_url(requested_url) else {
      return Err(Error::Other(format!(
        "web fetch same-origin mode requires a parseable URL origin; blocking request to {requested_url:?}"
      )));
    };
    if !client_origin.same_origin(&target_origin) {
      return Err(Error::Other(format!(
        "web fetch same-origin mode blocked cross-origin request from {client_origin} to {requested_url}"
      )));
    }
  }

  let referrer_policy = effective_referrer_policy(request, ctx);
  let credentials_mode = FetchCredentialsMode::from(request.credentials);

  // `FetchDestination` encodes both the request "destination" and a coarse request mode via
  // `sec_fetch_mode()`. For Fetch API requests, `FetchDestination::Fetch` represents the typical
  // `mode: "cors"` behavior, while `FetchDestination::Other` matches `no-cors` subresource
  // semantics.
  //
  // When the Web Fetch `Request.mode` is `no-cors`, ensure we use the `Other` profile so the
  // underlying HTTP layer:
  // - emits `Sec-Fetch-Mode: no-cors`
  // - omits `Origin` (consistent with other no-cors fetches)
  // - skips CORS preflight logic that is specific to CORS-mode requests
  let destination = match (ctx.destination, request.mode) {
    (FetchDestination::Fetch, RequestMode::NoCors) => FetchDestination::Other,
    (other, _) => other,
  };
  let fetch_request = FetchRequest {
    url: requested_url,
    destination,
    referrer_url,
    client_origin,
    referrer_policy,
    credentials_mode,
  };

  let mut request_headers = Headers::new_with_guard_and_limits(
    match request.mode {
      RequestMode::NoCors => HeadersGuard::RequestNoCors,
      _ => HeadersGuard::Request,
    },
    request.headers.limits(),
  );
  // Re-apply the appropriate guard for the current request mode so callers don't have to keep the
  // `Headers` guard in sync when they mutate `Request.mode` directly.
  request_headers
    .fill_from_pairs(request.headers.raw_pairs())
    .map_err(|err| Error::Other(err.to_string()))?;
  let user_header_pairs = request_headers.raw_pairs();

  let body_bytes = request.body.as_ref().map(|body| body.as_bytes());

  let http_req = HttpRequest {
    fetch: fetch_request,
    method,
    redirect: request.redirect,
    headers: &user_header_pairs,
    body: body_bytes,
  };

  // Cacheable GET fast path: allow `ResourceFetcher` implementations to use the simpler
  // `fetch_with_request` path (which is typically what caching layers key off of).
  let mut resource = if method_is_get
    && user_header_pairs.is_empty()
    && body_bytes.is_none()
    && request.redirect == RequestRedirect::Follow
  {
    fetcher.fetch_with_request(fetch_request)?
  } else {
    fetcher.fetch_http_request(http_req)?
  };

  // Canonicalize the final URL via URL parsing so later comparisons follow URL-record semantics
  // instead of raw string equality.
  if let Some(final_url) = resource.final_url.as_deref() {
    let final_url_len = final_url.len();
    if final_url_len > max_url_bytes {
      return Err(Error::Other(format!(
        "web fetch final URL exceeds max_url_bytes (len={final_url_len}, limit={max_url_bytes})"
      )));
    }
    if let Ok(url) = Url::parse(final_url) {
      let canonical = url.as_str();
      let canonical_len = canonical.len();
      if canonical_len > max_url_bytes {
        return Err(Error::Other(format!(
          "web fetch final URL exceeds max_url_bytes (len={canonical_len}, limit={max_url_bytes})"
        )));
      }
      if canonical != final_url {
        resource.final_url = Some(canonical.to_string());
      }
    }
  }

  let status = resource.status.unwrap_or(200);
  let final_url = resource.final_url.as_deref().unwrap_or(requested_url);
  // Fetch's "redirect status" list excludes 300 (Multiple Choices).
  // https://fetch.spec.whatwg.org/#redirect-status
  let redirect_status = matches!(status, 301 | 302 | 303 | 307 | 308);
  let redirect_detected = final_url != requested_url || redirect_status;

  match request.redirect {
    RequestRedirect::Follow => {}
    RequestRedirect::Error => {
      if redirect_detected {
        return Err(Error::Other(format!(
          "web fetch redirect mode is \"error\" but a redirect was detected ({requested_url} -> {final_url})"
        )));
      }
    }
    RequestRedirect::Manual => {
      if redirect_detected {
        if request.mode == RequestMode::SameOrigin {
          let Some(client_origin) = client_origin else {
            return Err(Error::Other(
              "web fetch same-origin mode requires a client origin; blocking request".to_string(),
            ));
          };
          let Some(target_origin) = origin_from_url(final_url) else {
            return Err(Error::Other(format!(
              "web fetch same-origin mode requires a parseable URL origin; blocking request to {final_url:?}"
            )));
          };
          if !client_origin.same_origin(&target_origin) {
            return Err(Error::Other(format!(
              "web fetch same-origin mode blocked cross-origin redirect from {client_origin} to {final_url}"
            )));
          }
        }
        if let Some(csp) = ctx.csp {
          let parsed = Url::parse(final_url).map_err(|_| {
            Error::Other(format!(
              "Blocked by Content-Security-Policy ({}) for final URL (invalid URL): {}",
              CspDirective::ConnectSrc.as_str(),
              final_url
            ))
          })?;
          if !csp.allows_url(CspDirective::ConnectSrc, client_origin, &parsed) {
            return Err(Error::Other(format!(
              "Blocked by Content-Security-Policy ({}) for final URL: {}",
              CspDirective::ConnectSrc.as_str(),
              parsed.as_str()
            )));
          }
        }
        return Ok(opaque_response(ResponseType::OpaqueRedirect));
      }
    }
  }

  if request.mode == RequestMode::SameOrigin {
    let Some(client_origin) = client_origin else {
      return Err(Error::Other(
        "web fetch same-origin mode requires a client origin; blocking request".to_string(),
      ));
    };
    let Some(target_origin) = origin_from_url(final_url) else {
      return Err(Error::Other(format!(
        "web fetch same-origin mode requires a parseable URL origin; blocking request to {final_url:?}"
      )));
    };
    if !client_origin.same_origin(&target_origin) {
      return Err(Error::Other(format!(
        "web fetch same-origin mode blocked cross-origin redirect from {client_origin} to {final_url}"
      )));
    }
  }

  if request.mode == RequestMode::Cors {
    // Fetch API `mode: "cors"` requests always validate CORS headers when we know the initiating
    // client origin. (Subresource CORS enforcement can be disabled via `FASTR_FETCH_ENFORCE_CORS`,
    // but `fetch()` should behave like browsers by default.)
    if let Err(message) =
      validate_cors_allow_origin(&resource, requested_url, client_origin, credentials_mode)
    {
      let mut err =
        ResourceError::new(requested_url, message).with_content_type(resource.content_type.clone());
      if let Some(status) = resource.status {
        err = err.with_status(status);
      }
      if let Some(final_url) = resource.final_url.as_deref() {
        err = err.with_final_url(final_url.to_string());
      }
      return Err(Error::Resource(err));
    }
  }

  if method_is_head {
    resource.bytes.clear();
  }
  let url = resource
    .final_url
    .take()
    .unwrap_or_else(|| requested_url.to_string());
  let redirected = url != requested_url;

  if let Some(csp) = ctx.csp {
    let parsed = Url::parse(url.as_str()).map_err(|_| {
      Error::Other(format!(
        "Blocked by Content-Security-Policy ({}) for final URL (invalid URL): {}",
        CspDirective::ConnectSrc.as_str(),
        url
      ))
    })?;
    if !csp.allows_url(CspDirective::ConnectSrc, client_origin, &parsed) {
      return Err(Error::Other(format!(
        "Blocked by Content-Security-Policy ({}) for final URL: {}",
        CspDirective::ConnectSrc.as_str(),
        parsed.as_str()
      )));
    }
  }

  let mut headers =
    Headers::new_with_guard_and_limits(HeadersGuard::Response, request.headers.limits());
  if let Some(response_headers) = resource.response_headers.take() {
    for (name, value) in response_headers {
      if let Err(err) = headers.append(&name, &value) {
        if matches!(err, WebFetchError::LimitExceeded { .. }) {
          return Err(Error::Other(format!(
            "web fetch response headers exceed configured limits: {err}"
          )));
        }
        if runtime::runtime_toggles().truthy("FASTR_WEB_FETCH_DEBUG") {
          eprintln!("web fetch: skipping invalid response header {name:?}: {value:?} ({err})");
        }
      }
    }
  }

  // Fetch: "null body status" responses must have a `null` body, regardless of any bytes returned
  // by the underlying fetcher.
  // https://fetch.spec.whatwg.org/#null-body-status
  let null_body_status = matches!(status, 101 | 103 | 204 | 205 | 304);
  let body = if method.eq_ignore_ascii_case("HEAD") || null_body_status {
    None
  } else {
    Some(
      Body::new_response(
        std::mem::take(&mut resource.bytes),
        request.headers.limits(),
      )
      .map_err(|err| {
        Error::Other(format!(
          "web fetch response body exceeds configured limits: {err}"
        ))
      })?,
    )
  };

  let mut response = Response {
    // NOTE: Fetch uses response "tainting" to decide whether to expose a basic/cors/opaque response
    // surface. We build the underlying response first, then apply the filtered response shape
    // based on the request mode below.
    r#type: ResponseType::Default,
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
  };

  // Apply Fetch response tainting based on request mode / redirect behavior.
  // https://fetch.spec.whatwg.org/#concept-filtered-response
  // https://fetch.spec.whatwg.org/#redirect-status
  let status_is_redirect = matches!(response.status, 301 | 302 | 303 | 307 | 308);
  let response_type = if request.redirect == RequestRedirect::Manual && status_is_redirect {
    ResponseType::OpaqueRedirect
  } else {
    match request.mode {
      RequestMode::Cors => match (client_origin, origin_from_url(response.url.as_str())) {
        (Some(client_origin), Some(target_origin)) => {
          if client_origin.same_origin(&target_origin) {
            ResponseType::Basic
          } else {
            ResponseType::Cors
          }
        }
        _ => ResponseType::Default,
      },
      RequestMode::SameOrigin | RequestMode::Navigate => ResponseType::Basic,
      RequestMode::NoCors => ResponseType::Opaque,
    }
  };

  if matches!(
    response_type,
    ResponseType::Opaque | ResponseType::OpaqueRedirect
  ) {
    return Ok(opaque_response(response_type));
  }

  response.r#type = response_type;
  if response_type == ResponseType::Cors {
    // https://fetch.spec.whatwg.org/#concept-filtered-response-cors
    // Expose only the CORS-safelisted response headers plus any names listed in
    // `Access-Control-Expose-Headers`.
    let mut exposed: HashSet<String> = HashSet::new();
    let mut expose_all = false;
    if let Some(expose_header) = response
      .headers
      .get("access-control-expose-headers")
      .map_err(|err| Error::Other(err.to_string()))?
    {
      for token in expose_header.split(',') {
        let token = trim_http_whitespace(token);
        if token.is_empty() {
          continue;
        }
        if token == "*" {
          expose_all = true;
          continue;
        }
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
          exposed.insert(name.as_str().to_string());
        }
      }
    }

    // `*` does not apply for credentialed requests.
    let allow_all = expose_all && request.credentials != RequestCredentials::Include;
    let mut filtered =
      Headers::new_with_guard_and_limits(HeadersGuard::Response, response.headers.limits());
    for (name, value) in response.headers.raw_pairs() {
      if allow_all
        || is_cors_safelisted_response_header_name(&name)
        || exposed.contains(name.as_str())
      {
        filtered
          .append(&name, &value)
          .map_err(|err| Error::Other(err.to_string()))?;
      }
    }
    response.headers = filtered;
  }
  response.headers.set_guard(HeadersGuard::Immutable);

  Ok(response)
}

fn opaque_response(r#type: ResponseType) -> Response {
  // Opaque / opaque-redirect responses have an empty URL list in the Fetch spec, so `url` and
  // `redirected` are not observable by callers.
  Response {
    r#type,
    url: String::new(),
    redirected: false,
    status: 0,
    status_text: String::new(),
    headers: Headers::new_with_guard(HeadersGuard::Immutable),
    body: None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::web_fetch::RequestCredentials;
  use crate::resource::web_fetch::{WebFetchError, WebFetchLimits};
  use crate::resource::{origin_from_url, FetchedResource, HttpFetcher, HttpRetryPolicy};
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};
  use std::thread;
  use std::time::{Duration, Instant};

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
    let start = std::time::Instant::now();
    loop {
      let read = loop {
        match stream.read(&mut tmp) {
          Ok(read) => break read,
          Err(err)
            if matches!(
              err.kind(),
              std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
          {
            if start.elapsed() > Duration::from_secs(2) {
              panic!("timed out while reading request headers: {err}");
            }
            continue;
          }
          Err(err) => panic!("read request: {err}"),
        }
      };
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
      let len = len_line["content-length:".len()..]
        .trim()
        .parse::<usize>()
        .unwrap();
      while body.len() < len {
        let read = loop {
          match stream.read(&mut tmp) {
            Ok(read) => break read,
            Err(err)
              if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
              ) =>
            {
              if start.elapsed() > Duration::from_secs(2) {
                panic!("timed out while reading request body: {err}");
              }
              continue;
            }
            Err(err) => panic!("read request body: {err}"),
          }
        };
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
        let read = loop {
          match stream.read(&mut tmp) {
            Ok(read) => break read,
            Err(err)
              if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
              ) =>
            {
              if start.elapsed() > Duration::from_secs(2) {
                panic!("timed out while reading chunked request body: {err}");
              }
              continue;
            }
            Err(err) => panic!("read chunked body: {err}"),
          }
        };
        if read == 0 {
          break;
        }
        body.extend_from_slice(&tmp[..read]);
      }
      panic!("incomplete chunked body");
    }

    (header_str, body)
  }

  fn accept_http_stream(listener: &TcpListener, test_name: &str) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
      match listener.accept() {
        Ok((stream, _)) => return stream,
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(err) => panic!("{test_name}: accept failed: {err}"),
      }
    }
    panic!("{test_name}: timed out while waiting for incoming connection");
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
  fn absolute_request_urls_are_canonicalized() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"hello".to_vec(), None),
    };
    // URL parsing/serialization canonicalizes the trailing slash for empty paths.
    let request = Request::new("GET", "https://EXAMPLE.com");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.url, "https://example.com/");
    assert!(!response.redirected);
  }

  #[test]
  fn normalized_absolute_urls_are_canonicalized() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"hello".to_vec(), None),
    };
    // `resolve_request_url` normalizes invalid URLs (spaces/pipes) before parsing; ensure we still
    // apply URL-record canonicalization to the result (e.g. host casing).
    let request = Request::new("GET", "https://EXAMPLE.com/a b");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.url, "https://example.com/a%20b");
    assert!(!response.redirected);
  }

  #[test]
  fn request_urls_strip_fragments() {
    struct UrlAssertingFetcher;

    impl ResourceFetcher for UrlAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.url, "https://example.com/a");
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        panic!("fetch_http_request should not be called for cacheable GET requests");
      }
    }

    let fetcher = UrlAssertingFetcher;
    let request = Request::new("GET", "https://example.com/a#frag");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.url, "https://example.com/a");
  }

  #[test]
  fn url_base_for_origin_brackets_ipv6_hosts() {
    let origin = origin_from_url("https://[::1]/").expect("origin");
    let base = url_base_for_origin(&origin).expect("base url");
    assert_eq!(base.as_str(), "https://[::1]/");
  }

  #[test]
  fn url_base_for_origin_brackets_ipv6_hosts_with_ports() {
    let origin = origin_from_url("https://[::1]:444/").expect("origin");
    let base = url_base_for_origin(&origin).expect("base url");
    assert_eq!(base.as_str(), "https://[::1]:444/");
  }

  #[test]
  fn redirected_is_false_when_final_url_only_differs_by_serialization() {
    let mut resource = FetchedResource::new(b"hello".to_vec(), None);
    resource.final_url = Some("https://example.com/".to_string());
    let fetcher = StaticFetcher { resource };
    // Browsers treat this as the same URL as "https://example.com/" (no redirect should be
    // observable via `Response.redirected`).
    let request = Request::new("GET", "https://example.com");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.url, "https://example.com/");
    assert!(!response.redirected);
  }

  #[test]
  fn final_url_is_canonicalized_before_redirect_detection() {
    struct RedirectAwareFetcher {
      resource: FetchedResource,
    }

    impl ResourceFetcher for RedirectAwareFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.redirect, RequestRedirect::Error);
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"hello".to_vec(), None);
    resource.final_url = Some("https://EXAMPLE.com/".to_string());
    let fetcher = RedirectAwareFetcher { resource };
    let mut request = Request::new("GET", "https://example.com/");
    request.redirect = RequestRedirect::Error;
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.url, "https://example.com/");
    assert!(!response.redirected);
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
    // Use a basic response surface so we can assert response headers directly (CORS responses are
    // filtered by `Access-Control-Expose-Headers`).
    let mut request = Request::new("GET", "https://example.com/a");
    request.set_mode(RequestMode::Navigate);
    let mut response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert_eq!(
      response.headers.get("x-test").unwrap().as_deref(),
      Some("hello")
    );
    assert!(!response.headers.has("set-cookie").unwrap());

    let err = response.headers.set("x-new", "blocked").unwrap_err();
    assert!(matches!(err, WebFetchError::HeadersImmutable));
  }

  #[test]
  fn response_headers_are_immutable() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let mut response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    let err = response.headers.set("x-test", "a").unwrap_err();
    assert!(matches!(err, WebFetchError::HeadersImmutable));
  }

  #[test]
  fn redirect_status_excludes_300_for_redirect_manual() {
    struct Status300Fetcher;

    impl ResourceFetcher for Status300Fetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request")
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, "GET");
        assert_eq!(req.redirect, RequestRedirect::Manual);
        let mut resource = FetchedResource::new(b"ok".to_vec(), None);
        resource.status = Some(300);
        Ok(resource)
      }
    }

    let fetcher = Status300Fetcher;
    let mut request = Request::new("GET", "https://example.com/start");
    request.set_mode(RequestMode::Navigate);
    request.redirect = RequestRedirect::Manual;

    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.r#type, ResponseType::Basic);
    assert_eq!(response.status, 300);
    assert_eq!(response.url, "https://example.com/start");
    assert!(!response.redirected);
  }

  #[test]
  fn response_type_basic_vs_cors() {
    let origin = origin_from_url("https://example.com/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Basic);

    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("*".to_string());
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://other.example/res");
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Cors);
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
  fn unsupported_fetcher_errors_for_custom_method() {
    let fetcher = PanicFetcher;
    let request = Request::new("PUT", "https://example.com/a");
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Resource(_)));
    assert!(err
      .to_string()
      .contains("does not support arbitrary HTTP requests"));
  }

  #[test]
  fn forbidden_method_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let request = Request::new("TRACE", "https://example.com/a");
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("forbidden"));
  }

  #[test]
  fn invalid_method_token_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let request = Request::new("GET /", "https://example.com/a");
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("method token"));
  }

  #[test]
  fn standard_methods_are_normalized_to_uppercase() {
    struct AssertingFetcher;

    impl ResourceFetcher for AssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, "GET");
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }
    }

    let fetcher = AssertingFetcher;
    let mut request = Request::new("get", "https://example.com/a");
    // Avoid the cacheable GET fast path (which calls `fetch_with_request`).
    request.headers.append("X-Test", "a").unwrap();
    execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
  }

  #[test]
  fn forwards_method_headers_and_body_to_fetch_http_request() {
    struct AssertingFetcher;

    impl ResourceFetcher for AssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, "PUT");
        assert_eq!(req.redirect, RequestRedirect::Manual);
        assert_eq!(req.body.expect("expected body"), b"hello");

        assert_eq!(req.headers.len(), 3);
        assert_eq!(req.headers[0].0, "x-test");
        assert_eq!(req.headers[0].1, "a");
        assert_eq!(req.headers[1].0, "x-other");
        assert_eq!(req.headers[1].1, "c");
        assert_eq!(req.headers[2].0, "x-test");
        assert_eq!(req.headers[2].1, "b");

        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }
    }

    let fetcher = AssertingFetcher;
    let mut request = Request::new("PUT", "https://example.com/a");
    request.redirect = RequestRedirect::Manual;
    request.headers.append("X-Test", "a").unwrap();
    request.headers.append("X-Other", "c").unwrap();
    request.headers.append("X-Test", "b").unwrap();
    request.body = Some(Body::new(b"hello".to_vec()).unwrap());

    execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
  }

  #[test]
  fn request_body_on_get_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "https://example.com/a");
    request.body = Some(Body::new(b"x".to_vec()).unwrap());
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn request_body_on_head_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("HEAD", "https://example.com/a");
    request.body = Some(Body::new(b"x".to_vec()).unwrap());
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn response_body_respects_web_fetch_limits() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(vec![0u8; 4], None),
    };
    let limits = WebFetchLimits {
      max_response_body_bytes: 3,
      ..WebFetchLimits::default()
    };
    let request = Request::new_with_limits("GET", "https://example.com/a", &limits);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("ResponseBodyBytes"));
  }

  #[test]
  fn request_url_over_max_url_bytes_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let limits = WebFetchLimits {
      max_url_bytes: 5,
      ..WebFetchLimits::default()
    };
    let request = Request::new_with_limits("GET", "https://example.com/a", &limits);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("max_url_bytes"));
  }

  #[test]
  fn final_url_over_max_url_bytes_errors() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some(format!("https://example.com/{}", "a".repeat(40)));
    let fetcher = StaticFetcher { resource };
    let limits = WebFetchLimits {
      max_url_bytes: 32,
      ..WebFetchLimits::default()
    };
    let request = Request::new_with_limits("GET", "https://example.com/a", &limits);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("final URL") && err.to_string().contains("max_url_bytes"));
  }

  #[test]
  fn skips_invalid_response_headers() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.response_headers = Some(vec![
      ("bad header".to_string(), "x".to_string()),
      ("x-ok".to_string(), "y".to_string()),
    ]);
    let fetcher = StaticFetcher { resource };
    // Use a basic response surface so we can observe the raw header list (CORS responses are
    // filtered by `Access-Control-Expose-Headers`).
    let mut request = Request::new("GET", "https://example.com/a");
    request.set_mode(RequestMode::Navigate);
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
    let fetcher = StaticFetcher {
      resource: resource.clone(),
    };
    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Include;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let err = execute_web_fetch(&fetcher, &request, ctx)
      .expect_err("expected credentialed CORS to require allow-credentials");
    assert!(err.to_string().contains("Access-Control-Allow-Credentials"));

    resource.access_control_allow_credentials = true;
    let fetcher = StaticFetcher { resource };
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    execute_web_fetch(&fetcher, &request, ctx).expect("expected credentialed CORS pass");
  }

  #[test]
  fn cors_preflight_rejects_invalid_allow_headers_token() {
    if skip_if_curl_backend_missing("cors_preflight_rejects_invalid_allow_headers_token") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_rejects_invalid_allow_headers_token")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options "),
        "expected preflight OPTIONS request, got:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-method: put"),
        "missing Access-Control-Request-Method header:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-headers: x-test"),
        "missing Access-Control-Request-Headers header:\n{headers}"
      );
      assert!(
        lower.contains("accept: */*"),
        "missing Accept header:\n{headers}"
      );
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: https://client.example\r\n",
        "Access-Control-Allow-Methods: PUT\r\n",
        "Access-Control-Allow-Headers: @@@\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure no follow-up request arrives.
      listener.set_nonblocking(true).unwrap();
      let start = std::time::Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected actual request after failed CORS preflight"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after preflight: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/preflight");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected preflight failure");
    let message = err.to_string();
    assert!(
      message.contains("preflight") && message.contains("Access-Control-Allow-Headers"),
      "unexpected error: {message}"
    );
    handle.join().unwrap();
  }

  #[test]
  fn cors_preflight_rejects_invalid_allow_methods_token() {
    if skip_if_curl_backend_missing("cors_preflight_rejects_invalid_allow_methods_token") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_rejects_invalid_allow_methods_token")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options "),
        "expected preflight OPTIONS request, got:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-method: put"),
        "missing Access-Control-Request-Method header:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-headers: x-test"),
        "missing Access-Control-Request-Headers header:\n{headers}"
      );
      assert!(
        lower.contains("accept: */*"),
        "missing Accept header:\n{headers}"
      );
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: https://client.example\r\n",
        "Access-Control-Allow-Methods: INV@LID\r\n",
        "Access-Control-Allow-Headers: x-test\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure no follow-up request arrives.
      listener.set_nonblocking(true).unwrap();
      let start = std::time::Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected actual request after failed CORS preflight"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after preflight: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/preflight");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected preflight failure");
    let message = err.to_string();
    assert!(
      message.contains("preflight") && message.contains("Access-Control-Allow-Methods"),
      "unexpected error: {message}"
    );
    handle.join().unwrap();
  }

  #[test]
  fn cors_preflight_applies_to_options_method() {
    if skip_if_curl_backend_missing("cors_preflight_applies_to_options_method") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_applies_to_options_method") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // Preflight request.
      let mut stream = accept_http_stream(&listener, "cors_preflight_applies_to_options_method");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options /opt"),
        "expected preflight OPTIONS request, got:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-method: options"),
        "missing Access-Control-Request-Method header:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-headers: x-test"),
        "missing Access-Control-Request-Headers header:\n{headers}"
      );
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: https://client.example\r\n",
        "Access-Control-Allow-Methods: OPTIONS\r\n",
        "Access-Control-Allow-Headers: x-test\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let mut stream = accept_http_stream(&listener, "cors_preflight_applies_to_options_method");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options /opt"),
        "expected actual OPTIONS request after preflight, got:\n{headers}"
      );
      assert!(
        !lower.contains("access-control-request-method:"),
        "unexpected Access-Control-Request-Method header on actual request:\n{headers}"
      );
      assert!(
        lower.contains("x-test: hello"),
        "missing X-Test header on actual request:\n{headers}"
      );
      let body = b"ok";
      let response = format!(
        concat!(
          "HTTP/1.1 200 OK\r\n",
          "Access-Control-Allow-Origin: https://client.example\r\n",
          "Content-Type: text/plain\r\n",
          "Content-Length: {}\r\n",
          "Connection: close\r\n",
          "\r\n"
        ),
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/opt");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut request = Request::new("OPTIONS", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();
  }

  #[test]
  fn cors_filters_response_headers_using_expose_headers() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    resource.response_headers = Some(vec![
      ("content-type".to_string(), "text/plain".to_string()),
      ("x-hidden".to_string(), "no".to_string()),
      ("x-exposed".to_string(), "yes".to_string()),
      (
        "access-control-expose-headers".to_string(),
        "x-exposed".to_string(),
      ),
      (
        "access-control-allow-origin".to_string(),
        "https://client.example".to_string(),
      ),
    ]);
    let fetcher = StaticFetcher { resource };

    let origin = origin_from_url("https://client.example/").expect("origin");
    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected CORS response");

    assert_eq!(response.r#type, ResponseType::Cors);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert_eq!(
      response.headers.get("x-exposed").unwrap().as_deref(),
      Some("yes")
    );
    assert_eq!(response.headers.get("x-hidden").unwrap(), None);
    assert_eq!(
      response.headers.get("access-control-allow-origin").unwrap(),
      None
    );
  }

  #[test]
  fn cors_response_filters_headers() {
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut exposed_resource = FetchedResource::new(b"ok".to_vec(), None);
    exposed_resource.access_control_allow_origin = Some("*".to_string());
    exposed_resource.response_headers = Some(vec![
      ("Content-Type".to_string(), "text/plain".to_string()),
      ("X-Test".to_string(), "a".to_string()),
      (
        "Access-Control-Expose-Headers".to_string(),
        "X-Test".to_string(),
      ),
    ]);
    let fetcher = StaticFetcher {
      resource: exposed_resource,
    };
    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Cors);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert_eq!(
      response.headers.get("x-test").unwrap().as_deref(),
      Some("a")
    );

    let mut unexposed_resource = FetchedResource::new(b"ok".to_vec(), None);
    unexposed_resource.access_control_allow_origin = Some("*".to_string());
    unexposed_resource.response_headers = Some(vec![
      ("Content-Type".to_string(), "text/plain".to_string()),
      ("X-Test".to_string(), "a".to_string()),
    ]);
    let fetcher = StaticFetcher {
      resource: unexposed_resource,
    };
    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Cors);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert_eq!(response.headers.get("x-test").unwrap(), None);
  }

  #[test]
  fn cors_expose_headers_wildcard_exposes_all_only_for_anonymous() {
    let origin = origin_from_url("https://client.example/").expect("origin");

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    resource.access_control_allow_credentials = true;
    resource.response_headers = Some(vec![
      ("x-hidden".to_string(), "no".to_string()),
      ("access-control-expose-headers".to_string(), "*".to_string()),
    ]);
    let fetcher = StaticFetcher {
      resource: resource.clone(),
    };

    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Omit;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response =
      execute_web_fetch(&fetcher, &request, ctx).expect("expected wildcard expose pass");
    assert_eq!(
      response.headers.get("x-hidden").unwrap().as_deref(),
      Some("no")
    );

    let fetcher = StaticFetcher { resource };
    let mut request = Request::new("GET", "https://other.example/res");
    request.credentials = RequestCredentials::Include;
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response =
      execute_web_fetch(&fetcher, &request, ctx).expect("expected credentialed CORS response");
    assert_eq!(response.headers.get("x-hidden").unwrap(), None);
  }

  #[test]
  fn request_referrer_overrides_execution_context_referrer_url() {
    struct ReferrerAssertingFetcher {
      expected_referrer_url: Option<&'static str>,
      resource: FetchedResource,
    }

    impl ResourceFetcher for ReferrerAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request")
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.referrer_url, self.expected_referrer_url);
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("*".to_string());
    let fetcher = ReferrerAssertingFetcher {
      expected_referrer_url: Some("https://override.example/referrer"),
      resource,
    };

    let mut request = Request::new("GET", "https://example.com/a");
    request.referrer = "https://override.example/referrer".to_string();
    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://ctx.example/page"),
      ..WebFetchExecutionContext::default()
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn request_referrer_policy_overrides_execution_context_referrer_policy() {
    struct PolicyAssertingFetcher {
      expected_referrer_policy: crate::resource::ReferrerPolicy,
      resource: FetchedResource,
    }

    impl ResourceFetcher for PolicyAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request")
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.referrer_policy, self.expected_referrer_policy);
        Ok(self.resource.clone())
      }
    }

    let fetcher = PolicyAssertingFetcher {
      expected_referrer_policy: crate::resource::ReferrerPolicy::NoReferrerWhenDowngrade,
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };

    let mut request = Request::new("GET", "https://example.com/a");
    request.referrer_policy = crate::resource::ReferrerPolicy::NoReferrerWhenDowngrade;
    let ctx = WebFetchExecutionContext {
      referrer_policy: crate::resource::ReferrerPolicy::NoReferrer,
      ..WebFetchExecutionContext::default()
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn empty_request_referrer_falls_back_to_execution_context_referrer_url() {
    struct ReferrerAssertingFetcher {
      expected_referrer_url: Option<&'static str>,
      resource: FetchedResource,
    }

    impl ResourceFetcher for ReferrerAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request")
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.referrer_url, self.expected_referrer_url);
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.access_control_allow_origin = Some("*".to_string());
    let fetcher = ReferrerAssertingFetcher {
      expected_referrer_url: Some("https://ctx.example/page"),
      resource,
    };

    let request = Request::new("GET", "https://example.com/a");
    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://ctx.example/page"),
      ..WebFetchExecutionContext::default()
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn whitespace_request_referrer_does_not_fall_back_to_execution_context_referrer_url() {
    // When `Request.referrer` is non-empty (even if only whitespace), it must not implicitly fall
    // back to the execution context's referrer URL. This prevents invalid referrer strings from
    // silently changing the base URL used for relative request resolution.
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "sub");
    request.set_mode(RequestMode::NoCors);
    request.referrer = " ".to_string();
    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://example.com/dir/page"),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected error");
    assert!(
      err.to_string().contains("invalid or missing base URL"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn forwards_execution_context_to_fetch_request() {
    struct ContextAssertingFetcher {
      expected_destination: FetchDestination,
      expected_referrer_url: &'static str,
      expected_client_origin: DocumentOrigin,
      expected_referrer_policy: crate::resource::ReferrerPolicy,
      expected_credentials_mode: FetchCredentialsMode,
    }

    impl ResourceFetcher for ContextAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.destination, self.expected_destination);
        assert_eq!(req.referrer_url, Some(self.expected_referrer_url));
        assert_eq!(req.client_origin, Some(&self.expected_client_origin));
        assert_eq!(req.referrer_policy, self.expected_referrer_policy);
        assert_eq!(req.credentials_mode, self.expected_credentials_mode);
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        panic!("fetch_http_request should not be called for cacheable GET requests");
      }
    }

    let origin = origin_from_url("https://example.com/").expect("origin");
    let fetcher = ContextAssertingFetcher {
      expected_destination: FetchDestination::StyleCors,
      expected_referrer_url: "https://referrer.example/page",
      expected_client_origin: origin.clone(),
      expected_referrer_policy: crate::resource::ReferrerPolicy::StrictOriginWhenCrossOrigin,
      expected_credentials_mode: FetchCredentialsMode::Include,
    };

    let mut request = Request::new("GET", "https://example.com/a");
    request.credentials = RequestCredentials::Include;

    let ctx = WebFetchExecutionContext {
      destination: FetchDestination::StyleCors,
      referrer_url: Some("https://referrer.example/page"),
      client_origin: Some(&origin),
      referrer_policy: crate::resource::ReferrerPolicy::StrictOriginWhenCrossOrigin,
      csp: None,
    };

    execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
  }

  #[test]
  fn response_type_respects_request_mode() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };

    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    // Without a client origin, `fetch()` cannot determine same-origin / cross-origin semantics.
    // Use `ResponseType::Default` as a conservative fallback.
    assert_eq!(response.r#type, ResponseType::Default);

    let mut request = Request::new("GET", "https://example.com/a");
    request.set_mode(RequestMode::NoCors);
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.r#type, ResponseType::Opaque);
    assert_eq!(response.status, 0);
    assert_eq!(response.url, "");
    assert!(!response.redirected);
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert!(response.body.is_none());
  }

  #[test]
  fn no_cors_requires_redirect_follow() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "https://example.com/a");
    request.set_mode(RequestMode::NoCors);
    request.redirect = RequestRedirect::Manual;
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(
      err.to_string().contains("no-cors") && err.to_string().contains("follow"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn no_cors_requires_safelisted_method() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("PUT", "https://example.com/a");
    request.set_mode(RequestMode::NoCors);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(
      err.to_string().contains("no-cors") && err.to_string().contains("CORS-safelisted"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn no_cors_fetch_uses_other_destination_profile() {
    struct DestinationInspectFetcher;

    impl ResourceFetcher for DestinationInspectFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.fetch.destination, FetchDestination::Other);
        assert_eq!(req.method, "GET");
        assert!(req.body.is_none());
        assert!(
          req.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("accept")),
          "expected Accept header to be passed through as a user header"
        );
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }
    }

    let fetcher = DestinationInspectFetcher;
    let mut request = Request::new("GET", "https://example.com/a");
    // Add a safelisted header so `execute_web_fetch` must use the `fetch_http_request` path.
    request.headers.append("Accept", "text/plain").unwrap();
    request.set_mode(RequestMode::NoCors);
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.r#type, ResponseType::Opaque);
  }

  #[test]
  fn same_origin_mode_returns_basic_response_type() {
    let fetcher = StaticFetcher {
      resource: FetchedResource::new(b"ok".to_vec(), None),
    };

    let mut request = Request::new("GET", "https://client.example/res");
    request.set_mode(RequestMode::SameOrigin);
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Basic);
    assert_eq!(response.status, 200);
    assert_eq!(response.url, "https://client.example/res");
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
  }

  #[test]
  fn head_response_has_no_body() {
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
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    assert!(response.body.is_none());
  }

  #[test]
  fn null_body_status_204_has_no_body() {
    let mut resource = FetchedResource::new(b"should-be-ignored".to_vec(), None);
    resource.status = Some(204);
    resource.response_headers = Some(vec![("Content-Type".to_string(), "text/plain".to_string())]);
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.status, 204);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert!(response.body.is_none());
  }

  #[test]
  fn null_body_status_304_has_no_body() {
    let mut resource = FetchedResource::new(b"should-be-ignored".to_vec(), None);
    resource.status = Some(304);
    resource.response_headers = Some(vec![("Content-Type".to_string(), "text/plain".to_string())]);
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://example.com/a");
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");
    assert_eq!(response.status, 304);
    assert_eq!(
      response.headers.get("content-type").unwrap().as_deref(),
      Some("text/plain")
    );
    assert!(response.body.is_none());
  }

  #[test]
  fn cors_preflight_does_not_require_allow_methods_for_safelisted_post() {
    if skip_if_curl_backend_missing(
      "cors_preflight_does_not_require_allow_methods_for_safelisted_post",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_does_not_require_allow_methods_for_safelisted_post")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // Preflight OPTIONS request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("options "),
        "expected preflight OPTIONS request, got:\n{req}"
      );
      assert!(
        req.contains("access-control-request-method: post"),
        "expected Access-Control-Request-Method for preflight, got:\n{req}"
      );
      assert!(
        req.contains("access-control-request-headers: x-test"),
        "expected Access-Control-Request-Headers for preflight, got:\n{req}"
      );
      let response =
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: http://client.example\r\nAccess-Control-Allow-Headers: x-test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual POST request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("post "),
        "expected POST request after successful preflight, got:\n{req}"
      );
      assert!(
        req.contains("x-test: 1"),
        "expected user header, got:\n{req}"
      );
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cors-post");
    let mut request = Request::new("POST", &url);
    request.headers.append("X-Test", "1").unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();
  }

  #[test]
  fn cors_preflight_wildcard_allow_headers_does_not_cover_authorization() {
    if skip_if_curl_backend_missing(
      "cors_preflight_wildcard_allow_headers_does_not_cover_authorization",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_wildcard_allow_headers_does_not_cover_authorization")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // Preflight OPTIONS request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("options "),
        "expected preflight OPTIONS request, got:\n{req}"
      );
      assert!(
        req.contains("access-control-request-method: get"),
        "expected Access-Control-Request-Method for preflight, got:\n{req}"
      );
      assert!(
        req.contains("access-control-request-headers: authorization"),
        "expected Access-Control-Request-Headers for preflight, got:\n{req}"
      );
      let response =
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: http://client.example\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure the actual request is not sent.
      listener.set_nonblocking(true).unwrap();
      let start = std::time::Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected follow-up request after preflight failure"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after preflight: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cors-auth");
    let mut request = Request::new("GET", &url);
    request
      .headers
      .append("Authorization", "Bearer token")
      .unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected preflight failure");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("preflight") && msg.contains("authorization"),
      "unexpected error: {err}"
    );
    assert!(
      msg.contains("access-control-allow-headers"),
      "unexpected error: {err}"
    );
    handle.join().unwrap();
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
      let mut stream = accept_http_stream(
        &listener,
        "request_user_agent_header_is_ignored_over_network",
      );
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
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("x-test: hello"),
      "expected header, got:\n{req}"
    );
  }

  #[test]
  fn request_user_agent_header_is_ignored_over_network() {
    if skip_if_curl_backend_missing("request_user_agent_header_is_ignored_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("request_user_agent_header_is_ignored_over_network")
    else {
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

    let fetcher = test_http_fetcher().with_user_agent("UA1");
    let url = format!("http://{addr}/ua");
    let mut request = Request::new("GET", &url);
    request.headers.append("User-Agent", "UA2").unwrap();
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("user-agent: ua1"),
      "expected fetcher UA, got:\n{req}"
    );
    assert!(
      !req.contains("user-agent: ua2"),
      "expected request UA to be ignored, got:\n{req}"
    );
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
    request.body = Some(Body::new(b"hello".to_vec()).unwrap());
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();
    assert_eq!(&*captured.lock().unwrap(), b"hello");
  }

  #[test]
  fn sends_delete_body_over_network() {
    if skip_if_curl_backend_missing("sends_delete_body_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("sends_delete_body_over_network") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured_headers = Arc::new(Mutex::new(String::new()));
    let captured_body = Arc::new(Mutex::new(Vec::new()));
    let captured_headers_req = Arc::clone(&captured_headers);
    let captured_body_req = Arc::clone(&captured_body);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      *captured_headers_req.lock().unwrap() = headers;
      *captured_body_req.lock().unwrap() = body;
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/delete");
    let mut request = Request::new("DELETE", &url);
    request.body = Some(Body::new(b"hello".to_vec()).unwrap());
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let req = captured_headers.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.starts_with("delete /delete"),
      "expected DELETE request line, got:\n{req}"
    );
    assert_eq!(&*captured_body.lock().unwrap(), b"hello");
  }

  #[test]
  fn sends_custom_method_with_body_over_network() {
    if skip_if_curl_backend_missing("sends_custom_method_with_body_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("sends_custom_method_with_body_over_network") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured_headers = Arc::new(Mutex::new(String::new()));
    let captured_body = Arc::new(Mutex::new(Vec::new()));
    let captured_headers_req = Arc::clone(&captured_headers);
    let captured_body_req = Arc::clone(&captured_body);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      *captured_headers_req.lock().unwrap() = headers;
      *captured_body_req.lock().unwrap() = body;
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/propfind");
    let mut request = Request::new("PROPFIND", &url);
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let req = captured_headers.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.starts_with("propfind /propfind"),
      "expected PROPFIND request line, got:\n{req}"
    );
    assert_eq!(&*captured_body.lock().unwrap(), b"payload");
  }

  #[test]
  fn sends_custom_method_over_network() {
    if skip_if_curl_backend_missing("sends_custom_method_over_network") {
      return;
    }
    let Some(listener) = try_bind_localhost("sends_custom_method_over_network") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured_headers = Arc::new(Mutex::new(String::new()));
    let captured_body = Arc::new(Mutex::new(Vec::new()));
    let captured_headers_req = Arc::clone(&captured_headers);
    let captured_body_req = Arc::clone(&captured_body);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      *captured_headers_req.lock().unwrap() = headers;
      *captured_body_req.lock().unwrap() = body;
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/custom");
    let mut request = Request::new("PUT", &url);
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let req = captured_headers.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.starts_with("put /custom"),
      "expected PUT request line, got:\n{req}"
    );
    assert!(
      req.contains("x-test: hello"),
      "expected user header, got:\n{req}"
    );
    assert_eq!(&*captured_body.lock().unwrap(), b"payload");
  }

  #[test]
  fn cors_preflight_runs_for_redirect_followup() {
    if skip_if_curl_backend_missing("cors_preflight_runs_for_redirect_followup") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_runs_for_redirect_followup") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(&listener, "cors_preflight_runs_for_redirect_followup");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 302 Found\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("put /final"),
              "expected PUT /final request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + PUT /start + OPTIONS /final + PUT /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /final"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("put /final"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_skips_followup_when_303_switches_to_get() {
    if skip_if_curl_backend_missing("cors_preflight_skips_followup_when_303_switches_to_get") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_skips_followup_when_303_switches_to_get")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_skips_followup_when_303_switches_to_get",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: *\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 303 See Other\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted for the follow-up request after switching to GET.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected redirect follow-up to be simple)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS /start + PUT /start + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("get /final"),
      "request[2]: {}",
      lines[2]
    );
  }

  #[test]
  fn cors_redirect_suppresses_authorization_and_skips_preflight() {
    if skip_if_curl_backend_missing("cors_redirect_suppresses_authorization_and_skips_preflight") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_redirect_suppresses_authorization_and_skips_preflight")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_redirect_suppresses_authorization_and_skips_preflight",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: authorization"),
              "expected Authorization to be requested in preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: authorization\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("authorization: bearer test"),
              "expected Authorization header on initial request, got:\n{headers}"
            );
            let location = format!("http://localhost:{}/final", addr.port());
            let response = format!(
              "HTTP/1.1 303 See Other\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("authorization:"),
              "expected Authorization to be suppressed on redirect, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted on the follow-up request after Authorization is
      // suppressed due to a cross-origin redirect.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected redirect follow-up to be simple)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request
      .headers
      .append("Authorization", "Bearer test")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS /start + PUT /start + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("get /final"),
      "request[2]: {}",
      lines[2]
    );
  }

  #[test]
  fn cors_preflight_omits_redirect_suppressed_authorization() {
    if skip_if_curl_backend_missing("cors_preflight_omits_redirect_suppressed_authorization") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_omits_redirect_suppressed_authorization")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      let mut last_connection: Option<Instant> = None;
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            last_connection = Some(Instant::now());
            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let (headers, _body) = read_http_request(&mut stream);
            captured_req.lock().unwrap().push(headers.clone());
            let req_lower = headers.to_ascii_lowercase();
            if req_lower.starts_with("options /start") {
              assert!(
                req_lower.contains("access-control-request-method: put"),
                "expected Access-Control-Request-Method: PUT, got:\n{headers}"
              );
              assert!(
                req_lower.contains("access-control-request-headers: authorization"),
                "expected Authorization to be requested in preflight, got:\n{headers}"
              );
              assert!(
                req_lower.contains("x-test"),
                "expected X-Test to be requested in preflight, got:\n{headers}"
              );
              let response = concat!(
                "HTTP/1.1 204 No Content\r\n",
                "Access-Control-Allow-Origin: http://client.example\r\n",
                "Access-Control-Allow-Methods: PUT\r\n",
                "Access-Control-Allow-Headers: authorization, x-test\r\n",
                "Content-Length: 0\r\n",
                "Connection: close\r\n",
                "\r\n"
              );
              stream.write_all(response.as_bytes()).unwrap();
            } else if req_lower.starts_with("put /start") {
              assert!(
                req_lower.contains("authorization: bearer test"),
                "expected Authorization header on initial request, got:\n{headers}"
              );
              assert!(
                req_lower.contains("x-test: 1"),
                "expected X-Test header on initial request, got:\n{headers}"
              );
              let location = format!("http://localhost:{port}/final");
              let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
              );
              stream.write_all(response.as_bytes()).unwrap();
            } else if req_lower.starts_with("options /final") {
              assert!(
                req_lower.contains("access-control-request-method: put"),
                "expected Access-Control-Request-Method: PUT, got:\n{headers}"
              );
              assert!(
                req_lower.contains("access-control-request-headers: x-test")
                  && !req_lower.contains("authorization"),
                "expected follow-up preflight to request only X-Test (Authorization is redirect-suppressed), got:\n{headers}"
              );
              let response = concat!(
                "HTTP/1.1 204 No Content\r\n",
                "Access-Control-Allow-Origin: http://client.example\r\n",
                "Access-Control-Allow-Methods: PUT\r\n",
                "Access-Control-Allow-Headers: x-test\r\n",
                "Content-Length: 0\r\n",
                "Connection: close\r\n",
                "\r\n"
              );
              stream.write_all(response.as_bytes()).unwrap();
            } else if req_lower.starts_with("put /final") {
              assert!(
                req_lower.contains("x-test: 1"),
                "expected X-Test header on redirect follow-up request, got:\n{headers}"
              );
              assert!(
                !req_lower.contains("authorization:"),
                "expected Authorization to be suppressed on cross-origin redirect, got:\n{headers}"
              );
              let body = b"ok";
              let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              stream.write_all(response.as_bytes()).unwrap();
              stream.write_all(body).unwrap();
            } else {
              panic!("unexpected request:\n{headers}");
            }
          }
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            if last_connection.is_some_and(|last| last.elapsed() > Duration::from_millis(200)) {
              break;
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request
      .headers
      .append("Authorization", "Bearer test")
      .unwrap();
    request.headers.append("X-Test", "1").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + PUT /start + OPTIONS /final + PUT /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /final"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("put /final"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_redirect_suppresses_content_type_after_303() {
    if skip_if_curl_backend_missing("cors_redirect_suppresses_content_type_after_303") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_redirect_suppresses_content_type_after_303")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream =
          accept_http_stream(&listener, "cors_redirect_suppresses_content_type_after_303");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Content-Type to be requested in preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 303 See Other\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected Content-Type to be suppressed after switching to GET, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted on the follow-up request after Content-Type is
      // suppressed due to the 303 redirect mutation (PUT -> GET).
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected redirect follow-up to be simple)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS /start + PUT /start + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("get /final"),
      "request[2]: {}",
      lines[2]
    );
  }

  #[test]
  fn cors_preflight_uses_redirect_mutated_method_after_303() {
    if skip_if_curl_backend_missing("cors_preflight_uses_redirect_mutated_method_after_303") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_uses_redirect_mutated_method_after_303")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_uses_redirect_mutated_method_after_303",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: put"),
              "expected Access-Control-Request-Method: PUT, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: x-test"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 303 See Other\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: get"),
              "expected Access-Control-Request-Method: GET after 303 mutation, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: x-test"),
              "expected Access-Control-Request-Headers for follow-up preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: GET\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on follow-up request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.headers.append("X-Test", "1").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + PUT /start + OPTIONS /final + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /final"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("get /final"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_omits_redirect_suppressed_content_type_after_303() {
    if skip_if_curl_backend_missing(
      "cors_preflight_omits_redirect_suppressed_content_type_after_303",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_omits_redirect_suppressed_content_type_after_303")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_omits_redirect_suppressed_content_type_after_303",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: put"),
              "expected Access-Control-Request-Method: PUT, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type,x-test"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: content-type, x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("put /start"),
              "expected PUT /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on initial request, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 303 See Other\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: get"),
              "expected Access-Control-Request-Method: GET after 303 mutation, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: x-test")
                && !req_lower.contains("content-type"),
              "expected follow-up preflight to request only X-Test (Content-Type is redirect-suppressed), got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: GET\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on follow-up request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected Content-Type to be suppressed after switching to GET, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.headers.append("X-Test", "1").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + PUT /start + OPTIONS /final + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /final"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("get /final"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_uses_redirect_mutated_method_after_302_post() {
    if skip_if_curl_backend_missing("cors_preflight_uses_redirect_mutated_method_after_302_post") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_uses_redirect_mutated_method_after_302_post")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_uses_redirect_mutated_method_after_302_post",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type,x-test"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type, x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 302 Found\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: get"),
              "expected Access-Control-Request-Method: GET after 302 POST->GET mutation, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: x-test")
                && !req_lower.contains("content-type"),
              "expected follow-up preflight to request only X-Test (Content-Type is redirect-suppressed), got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: GET\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on follow-up request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected Content-Type to be suppressed after switching to GET, got:\n{headers}"
            );
            assert!(
              body.is_empty(),
              "expected redirected GET to send no body, got {body:?}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.headers.append("X-Test", "1").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + POST /start + OPTIONS /final + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /start"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("post /start"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /final"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("get /final"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_skips_followup_when_302_post_switches_to_get() {
    if skip_if_curl_backend_missing("cors_preflight_skips_followup_when_302_post_switches_to_get") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_skips_followup_when_302_post_switches_to_get")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_skips_followup_when_302_post_switches_to_get",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 302 Found\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected redirected GET to drop Content-Type, got:\n{headers}"
            );
            assert!(
              body.is_empty(),
              "expected redirected GET to send no body, got {body:?}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted for the follow-up request after switching to GET.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected redirect follow-up to be simple)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS /start + POST /start + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(lines[0].starts_with("options /start"), "request[0]: {}", lines[0]);
    assert!(lines[1].starts_with("post /start"), "request[1]: {}", lines[1]);
    assert!(lines[2].starts_with("get /final"), "request[2]: {}", lines[2]);
  }

  #[test]
  fn cors_preflight_skips_followup_when_301_post_switches_to_get() {
    if skip_if_curl_backend_missing("cors_preflight_skips_followup_when_301_post_switches_to_get") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_skips_followup_when_301_post_switches_to_get")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_skips_followup_when_301_post_switches_to_get",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 301 Moved Permanently\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected redirected GET to drop Content-Type, got:\n{headers}"
            );
            assert!(
              body.is_empty(),
              "expected redirected GET to send no body, got {body:?}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted for the follow-up request after switching to GET.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected redirect follow-up to be simple)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS /start + POST /start + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(lines[0].starts_with("options /start"), "request[0]: {}", lines[0]);
    assert!(lines[1].starts_with("post /start"), "request[1]: {}", lines[1]);
    assert!(lines[2].starts_with("get /final"), "request[2]: {}", lines[2]);
  }

  #[test]
  fn cors_preflight_uses_redirect_mutated_method_after_301_post() {
    if skip_if_curl_backend_missing("cors_preflight_uses_redirect_mutated_method_after_301_post") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_uses_redirect_mutated_method_after_301_post")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_uses_redirect_mutated_method_after_301_post",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type,x-test"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type, x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 301 Moved Permanently\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: get"),
              "expected Access-Control-Request-Method: GET after 301 POST->GET mutation, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: x-test")
                && !req_lower.contains("content-type"),
              "expected follow-up preflight to request only X-Test (Content-Type is redirect-suppressed), got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: GET\r\n",
              "Access-Control-Allow-Headers: x-test\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("get /final"),
              "expected GET /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("x-test: 1"),
              "expected X-Test header on follow-up request, got:\n{headers}"
            );
            assert!(
              !req_lower.contains("content-type:"),
              "expected Content-Type to be suppressed after switching to GET, got:\n{headers}"
            );
            assert!(
              body.is_empty(),
              "expected redirected GET to send no body, got {body:?}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.headers.append("X-Test", "1").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + POST /start + OPTIONS /final + GET /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(lines[0].starts_with("options /start"), "request[0]: {}", lines[0]);
    assert!(lines[1].starts_with("post /start"), "request[1]: {}", lines[1]);
    assert!(lines[2].starts_with("options /final"), "request[2]: {}", lines[2]);
    assert!(lines[3].starts_with("get /final"), "request[3]: {}", lines[3]);
  }

  #[test]
  fn cors_preflight_preserves_post_on_307_redirect_followup() {
    if skip_if_curl_backend_missing("cors_preflight_preserves_post_on_307_redirect_followup") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_preserves_post_on_307_redirect_followup")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream =
          accept_http_stream(&listener, "cors_preflight_preserves_post_on_307_redirect_followup");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 307 Temporary Redirect\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST (307 preserves method), got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for follow-up preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("post /final"),
              "expected POST /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on follow-up request, got:\n{headers}"
            );
            assert_eq!(
              body,
              b"payload",
              "expected redirect follow-up POST to preserve the body"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }
 
      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });
 
    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
 
    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();
 
    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + POST /start + OPTIONS /final + POST /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(lines[0].starts_with("options /start"), "request[0]: {}", lines[0]);
    assert!(lines[1].starts_with("post /start"), "request[1]: {}", lines[1]);
    assert!(lines[2].starts_with("options /final"), "request[2]: {}", lines[2]);
    assert!(lines[3].starts_with("post /final"), "request[3]: {}", lines[3]);
  }

  #[test]
  fn cors_preflight_preserves_post_on_308_redirect_followup() {
    if skip_if_curl_backend_missing("cors_preflight_preserves_post_on_308_redirect_followup") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_preserves_post_on_308_redirect_followup")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream =
          accept_http_stream(&listener, "cors_preflight_preserves_post_on_308_redirect_followup");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /start"),
              "expected OPTIONS /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 => {
            assert!(
              req_lower.starts_with("post /start"),
              "expected POST /start request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on initial request, got:\n{headers}"
            );
            assert_eq!(body, b"payload", "expected POST body to be forwarded");
            let response = concat!(
              "HTTP/1.1 308 Permanent Redirect\r\n",
              "Location: /final\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          2 => {
            assert!(
              req_lower.starts_with("options /final"),
              "expected OPTIONS /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-method: post"),
              "expected Access-Control-Request-Method: POST (308 preserves method), got:\n{headers}"
            );
            assert!(
              req_lower.contains("access-control-request-headers: content-type"),
              "expected Access-Control-Request-Headers for follow-up preflight, got:\n{headers}"
            );
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: http://client.example\r\n",
              "Access-Control-Allow-Methods: POST\r\n",
              "Access-Control-Allow-Headers: content-type\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("post /final"),
              "expected POST /final request, got:\n{headers}"
            );
            assert!(
              req_lower.contains("content-type: application/json"),
              "expected Content-Type header on follow-up request, got:\n{headers}"
            );
            assert_eq!(
              body,
              b"payload",
              "expected redirect follow-up POST to preserve the body"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/start");
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_mut().unwrap().consume_bytes().unwrap(), b"ok");
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS /start + POST /start + OPTIONS /final + POST /final, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(lines[0].starts_with("options /start"), "request[0]: {}", lines[0]);
    assert!(lines[1].starts_with("post /start"), "request[1]: {}", lines[1]);
    assert!(lines[2].starts_with("options /final"), "request[2]: {}", lines[2]);
    assert!(lines[3].starts_with("post /final"), "request[3]: {}", lines[3]);
  }

  #[test]
  fn cors_preflight_cache_skips_second_options() {
    if skip_if_curl_backend_missing("cors_preflight_cache_skips_second_options") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_cache_skips_second_options") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(&listener, "cors_preflight_cache_skips_second_options");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /cache"),
              "expected OPTIONS request, got:\n{headers}"
            );
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nAccess-Control-Max-Age: 600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 | 2 => {
            assert!(
              req_lower.starts_with("put /cache"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected preflight to be cached)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cache");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    for _ in 0..2 {
      let mut request = Request::new("PUT", &url);
      request.headers.append("X-Test", "hello").unwrap();
      let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
      assert_eq!(response.status, 200);
      assert_eq!(
        response.body.as_mut().unwrap().consume_bytes().unwrap(),
        b"ok"
      );
    }

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS + PUT + PUT requests, got:\n{captured:#?}"
    );
    let first = captured[0].to_ascii_lowercase();
    let second = captured[1].to_ascii_lowercase();
    let third = captured[2].to_ascii_lowercase();
    assert!(
      first.starts_with("options /cache"),
      "first request: {first}"
    );
    assert!(second.starts_with("put /cache"), "second request: {second}");
    assert!(third.starts_with("put /cache"), "third request: {third}");
  }

  #[test]
  fn cors_preflight_cache_defaults_max_age_to_5_seconds() {
    if skip_if_curl_backend_missing("cors_preflight_cache_defaults_max_age_to_5_seconds") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_cache_defaults_max_age_to_5_seconds")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_defaults_max_age_to_5_seconds",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /cache"),
              "expected OPTIONS request, got:\n{headers}"
            );
            // No Access-Control-Max-Age header: Fetch defaults max-age to 5 seconds and the result
            // still populates the CORS-preflight cache.
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 | 2 => {
            assert!(
              req_lower.starts_with("put /cache"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected preflight to be cached)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cache");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    for _ in 0..2 {
      let mut request = Request::new("PUT", &url);
      request.headers.append("X-Test", "hello").unwrap();
      let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
      assert_eq!(response.status, 200);
      assert_eq!(
        response.body.as_mut().unwrap().consume_bytes().unwrap(),
        b"ok"
      );
    }

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS + PUT + PUT requests, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /cache"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /cache"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("put /cache"),
      "request[2]: {}",
      lines[2]
    );
  }

  #[test]
  fn cors_preflight_cache_defaults_max_age_to_5_seconds_for_invalid_header() {
    if skip_if_curl_backend_missing(
      "cors_preflight_cache_defaults_max_age_to_5_seconds_for_invalid_header",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_cache_defaults_max_age_to_5_seconds_for_invalid_header")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_defaults_max_age_to_5_seconds_for_invalid_header",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /invalid"),
              "expected OPTIONS request, got:\n{headers}"
            );
            // Invalid `Access-Control-Max-Age` header should default to 5 seconds (Fetch) and still
            // populate the CORS-preflight cache.
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nAccess-Control-Max-Age: invalid\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 | 2 => {
            assert!(
              req_lower.starts_with("put /invalid"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected preflight to be cached)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/invalid");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    for _ in 0..2 {
      let mut request = Request::new("PUT", &url);
      request.headers.append("X-Test", "hello").unwrap();
      let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
      assert_eq!(response.status, 200);
      assert_eq!(
        response.body.as_mut().unwrap().consume_bytes().unwrap(),
        b"ok"
      );
    }

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      3,
      "expected OPTIONS + PUT + PUT requests, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /invalid"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /invalid"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("put /invalid"),
      "request[2]: {}",
      lines[2]
    );
  }

  #[test]
  fn cors_preflight_cache_does_not_cache_with_max_age_zero() {
    if skip_if_curl_backend_missing("cors_preflight_cache_does_not_cache_with_max_age_zero") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_cache_does_not_cache_with_max_age_zero")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_does_not_cache_with_max_age_zero",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 | 2 => {
            assert!(
              req_lower.starts_with("options /cache"),
              "expected OPTIONS request, got:\n{headers}"
            );
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nAccess-Control-Max-Age: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 | 3 => {
            assert!(
              req_lower.starts_with("put /cache"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cache");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    for _ in 0..2 {
      let mut request = Request::new("PUT", &url);
      request.headers.append("X-Test", "hello").unwrap();
      let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
      assert_eq!(response.status, 200);
      assert_eq!(
        response.body.as_mut().unwrap().consume_bytes().unwrap(),
        b"ok"
      );
    }

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS + PUT + OPTIONS + PUT requests, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /cache"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /cache"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /cache"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("put /cache"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_cache_partitions_by_credentials() {
    if skip_if_curl_backend_missing("cors_preflight_cache_partitions_by_credentials") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_cache_partitions_by_credentials")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for idx in 0..5 {
        let mut stream =
          accept_http_stream(&listener, "cors_preflight_cache_partitions_by_credentials");
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let req_lower = headers.to_ascii_lowercase();
        match idx {
          0 => {
            assert!(
              req_lower.starts_with("options /cache"),
              "expected OPTIONS request, got:\n{headers}"
            );
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nAccess-Control-Max-Age: 600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          1 | 2 => {
            assert!(
              req_lower.starts_with("put /cache"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          3 => {
            assert!(
              req_lower.starts_with("options /cache"),
              "expected OPTIONS request, got:\n{headers}"
            );
            let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: X-Test\r\nAccess-Control-Max-Age: 600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
          }
          4 => {
            assert!(
              req_lower.starts_with("put /cache"),
              "expected PUT request, got:\n{headers}"
            );
            let body = b"ok";
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
          }
          _ => unreachable!(),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cache");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    // Anonymous (same-origin credentials mode is non-credentialed for cross-origin requests):
    // first request should run preflight, second should hit the cache.
    for _ in 0..2 {
      let mut request = Request::new("PUT", &url);
      request.headers.append("X-Test", "hello").unwrap();
      let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
      assert_eq!(response.status, 200);
      assert_eq!(
        response.body.as_mut().unwrap().consume_bytes().unwrap(),
        b"ok"
      );
    }

    // Switching to credentialed requests must not reuse the wildcard (`*`) cached preflight.
    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Include;
    request.headers.append("X-Test", "hello").unwrap();
    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      5,
      "expected OPTIONS + PUT + PUT + OPTIONS + PUT requests, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /cache"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /cache"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("put /cache"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("options /cache"),
      "request[3]: {}",
      lines[3]
    );
    assert!(
      lines[4].starts_with("put /cache"),
      "request[4]: {}",
      lines[4]
    );
  }

  #[test]
  fn cors_preflight_cache_credentialed_entry_matches_non_credentialed_request() {
    if skip_if_curl_backend_missing(
      "cors_preflight_cache_credentialed_entry_matches_non_credentialed_request",
    ) {
      return;
    }
    let Some(listener) = try_bind_localhost(
      "cors_preflight_cache_credentialed_entry_matches_non_credentialed_request",
    ) else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let options_count = Arc::new(AtomicUsize::new(0));
    let options_count_req = Arc::clone(&options_count);
    let handle = thread::spawn(move || {
      for _ in 0..3 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_credentialed_entry_matches_non_credentialed_request",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        let line = headers.lines().next().unwrap_or_default();
        let method = line.split_whitespace().next().unwrap_or_default();
        if method.eq_ignore_ascii_case("OPTIONS") {
          options_count_req.fetch_add(1, Ordering::SeqCst);
          let response = concat!(
            "HTTP/1.1 204 No Content\r\n",
            "Access-Control-Allow-Origin: https://client.example\r\n",
            "Access-Control-Allow-Credentials: true\r\n",
            "Access-Control-Allow-Methods: PUT\r\n",
            "Access-Control-Allow-Headers: x-test\r\n",
            "Access-Control-Max-Age: 60\r\n",
            "Content-Length: 0\r\n",
            "Connection: close\r\n",
            "\r\n"
          );
          stream.write_all(response.as_bytes()).unwrap();
        } else {
          let body = b"ok";
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(body).unwrap();
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected preflight to be cached)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cors");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Include;
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
    assert_eq!(
      options_count.load(Ordering::SeqCst),
      1,
      "expected credentialed preflight cache entry to match non-credentialed request"
    );
  }

  #[test]
  fn cors_preflight_cache_non_credentialed_entry_does_not_match_credentialed_request() {
    if skip_if_curl_backend_missing(
      "cors_preflight_cache_non_credentialed_entry_does_not_match_credentialed_request",
    ) {
      return;
    }
    let Some(listener) = try_bind_localhost(
      "cors_preflight_cache_non_credentialed_entry_does_not_match_credentialed_request",
    ) else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let options_count = Arc::new(AtomicUsize::new(0));
    let options_count_req = Arc::clone(&options_count);
    let handle = thread::spawn(move || {
      for _ in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_non_credentialed_entry_does_not_match_credentialed_request",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        let line = headers.lines().next().unwrap_or_default();
        let method = line.split_whitespace().next().unwrap_or_default();
        if method.eq_ignore_ascii_case("OPTIONS") {
          options_count_req.fetch_add(1, Ordering::SeqCst);
          let response = concat!(
            "HTTP/1.1 204 No Content\r\n",
            "Access-Control-Allow-Origin: https://client.example\r\n",
            "Access-Control-Allow-Credentials: true\r\n",
            "Access-Control-Allow-Methods: PUT\r\n",
            "Access-Control-Allow-Headers: x-test\r\n",
            "Access-Control-Max-Age: 60\r\n",
            "Content-Length: 0\r\n",
            "Connection: close\r\n",
            "\r\n"
          );
          stream.write_all(response.as_bytes()).unwrap();
        } else {
          let body = b"ok";
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(body).unwrap();
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected second preflight then stop)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cors");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Include;
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
    assert_eq!(
      options_count.load(Ordering::SeqCst),
      2,
      "expected non-credentialed preflight cache entry to not match credentialed request"
    );
  }

  #[test]
  fn cors_preflight_cache_wildcard_entries_do_not_match_credentialed_request() {
    if skip_if_curl_backend_missing(
      "cors_preflight_cache_wildcard_entries_do_not_match_credentialed_request",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_cache_wildcard_entries_do_not_match_credentialed_request")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      for _ in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_wildcard_entries_do_not_match_credentialed_request",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        captured_req.lock().unwrap().push(headers.clone());
        let line = headers.lines().next().unwrap_or_default();
        let method = line.split_whitespace().next().unwrap_or_default();
        if method.eq_ignore_ascii_case("OPTIONS") {
          let request_method = headers
            .lines()
            .find_map(|line| {
              let (name, value) = line.split_once(':')?;
              if name
                .trim()
                .eq_ignore_ascii_case("access-control-request-method")
              {
                Some(value.trim().to_string())
              } else {
                None
              }
            })
            .unwrap_or_default();
          if request_method.eq_ignore_ascii_case("PUT") {
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: https://client.example\r\n",
              "Access-Control-Allow-Credentials: true\r\n",
              "Access-Control-Allow-Methods: *, PUT\r\n",
              "Access-Control-Allow-Headers: *, x-test\r\n",
              "Access-Control-Max-Age: 600\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          } else if request_method.eq_ignore_ascii_case("DELETE") {
            let response = concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: https://client.example\r\n",
              "Access-Control-Allow-Credentials: true\r\n",
              "Access-Control-Allow-Methods: DELETE\r\n",
              "Access-Control-Max-Age: 600\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
          } else {
            panic!("unexpected Access-Control-Request-Method: {request_method:?}");
          }
        } else {
          let body = b"ok";
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(body).unwrap();
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/wildcard");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Include;
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    let mut request = Request::new("DELETE", &url);
    request.credentials = RequestCredentials::Include;
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      4,
      "expected OPTIONS + PUT + OPTIONS + DELETE requests, got:\n{captured:#?}"
    );
    let lines: Vec<String> = captured
      .iter()
      .map(|headers| headers.lines().next().unwrap_or("").to_ascii_lowercase())
      .collect();
    assert!(
      lines[0].starts_with("options /wildcard"),
      "request[0]: {}",
      lines[0]
    );
    assert!(
      lines[1].starts_with("put /wildcard"),
      "request[1]: {}",
      lines[1]
    );
    assert!(
      lines[2].starts_with("options /wildcard"),
      "request[2]: {}",
      lines[2]
    );
    assert!(
      lines[3].starts_with("delete /wildcard"),
      "request[3]: {}",
      lines[3]
    );
  }

  #[test]
  fn cors_preflight_cache_wildcard_header_entry_does_not_match_authorization() {
    if skip_if_curl_backend_missing(
      "cors_preflight_cache_wildcard_header_entry_does_not_match_authorization",
    ) {
      return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(listener) =
      try_bind_localhost("cors_preflight_cache_wildcard_header_entry_does_not_match_authorization")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let options_count = Arc::new(AtomicUsize::new(0));
    let options_count_req = Arc::clone(&options_count);
    let handle = thread::spawn(move || {
      for _ in 0..4 {
        let mut stream = accept_http_stream(
          &listener,
          "cors_preflight_cache_wildcard_header_entry_does_not_match_authorization",
        );
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let (headers, _body) = read_http_request(&mut stream);
        let line = headers.lines().next().unwrap_or_default();
        let method = line.split_whitespace().next().unwrap_or_default();
        if method.eq_ignore_ascii_case("OPTIONS") {
          options_count_req.fetch_add(1, Ordering::SeqCst);

          let header_lower = headers.to_ascii_lowercase();
          let req_headers = header_lower
            .lines()
            .find(|line| line.starts_with("access-control-request-headers:"))
            .map(|line| line["access-control-request-headers:".len()..].trim())
            .unwrap_or("");
          let wants_authorization = req_headers
            .split(',')
            .map(|token| token.trim())
            .any(|token| token == "authorization");

          let response = if wants_authorization {
            concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: https://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: authorization\r\n",
              "Access-Control-Max-Age: 60\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            )
          } else {
            concat!(
              "HTTP/1.1 204 No Content\r\n",
              "Access-Control-Allow-Origin: https://client.example\r\n",
              "Access-Control-Allow-Methods: PUT\r\n",
              "Access-Control-Allow-Headers: *\r\n",
              "Access-Control-Max-Age: 60\r\n",
              "Content-Length: 0\r\n",
              "Connection: close\r\n",
              "\r\n"
            )
          };
          stream.write_all(response.as_bytes()).unwrap();
        } else {
          let body = b"ok";
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(body).unwrap();
        }
      }

      // Ensure no extra preflight is attempted.
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected extra request (expected second preflight then stop)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after requests: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/cors");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    // First request caches `Access-Control-Allow-Headers: *`.
    let mut request = Request::new("PUT", &url);
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    // Second request includes Authorization, which must not match a wildcard header cache entry.
    let mut request = Request::new("PUT", &url);
    request
      .headers
      .append("Authorization", "Bearer test")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
    assert_eq!(
      options_count.load(Ordering::SeqCst),
      2,
      "expected Authorization request to trigger a second preflight (wildcard cache entry must not match)"
    );
  }

  #[test]
  fn cors_preflight_not_sent_for_simple_get_with_safelisted_header() {
    if skip_if_curl_backend_missing("cors_preflight_not_sent_for_simple_get_with_safelisted_header")
    {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_not_sent_for_simple_get_with_safelisted_header")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push(headers);
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      drop(stream);

      // Ensure no preflight request arrives (only a single GET should be sent).
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected second request (preflight should not be sent)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after request: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/simple");
    let mut request = Request::new("GET", &url);
    request.set_mode(RequestMode::Cors);
    // `Accept` is CORS-safelisted when it contains only safe bytes.
    request.headers.append("Accept", "text/plain").unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "expected one request, got {captured:?}");
    let req = captured[0].to_ascii_lowercase();
    assert!(
      req.starts_with("get /simple"),
      "expected GET request line, got:\n{req}"
    );
  }

  #[test]
  fn cors_preflight_not_sent_for_duplicate_safelisted_accept_headers() {
    if skip_if_curl_backend_missing(
      "cors_preflight_not_sent_for_duplicate_safelisted_accept_headers",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_not_sent_for_duplicate_safelisted_accept_headers")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();

      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      drop(stream);

      assert!(
        lower.starts_with("get /dupaccept"),
        "expected GET request line, got:\n{headers}"
      );

      // Ensure no preflight request arrives (only a single GET should be sent).
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected second request (preflight should not be sent)"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after request: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/dupaccept");
    let mut request = Request::new("GET", &url);
    request.set_mode(RequestMode::Cors);
    let value = "a".repeat(64);
    request.headers.append("Accept", &value).unwrap();
    request.headers.append("Accept", &value).unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
  }

  #[test]
  fn cors_preflight_sent_for_put_with_custom_header() {
    if skip_if_curl_backend_missing("cors_preflight_sent_for_put_with_custom_header") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_sent_for_put_with_custom_header")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // Preflight request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: http://client.example\r\n",
        "Access-Control-Allow-Methods: PUT\r\n",
        "Access-Control-Allow-Headers: x-test\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n",
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
      let body_bytes = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body_bytes).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/preflight");
    let mut request = Request::new("PUT", &url);
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");

    let preflight_headers = captured[0].0.to_ascii_lowercase();
    let preflight_body = &captured[0].1;
    assert!(
      preflight_headers.starts_with("options /preflight"),
      "unexpected preflight request:\n{preflight_headers}"
    );
    assert!(
      preflight_headers.contains("origin: http://client.example"),
      "missing Origin header:\n{preflight_headers}"
    );
    assert!(
      preflight_headers.contains("access-control-request-method: put"),
      "missing Access-Control-Request-Method header:\n{preflight_headers}"
    );
    assert!(
      preflight_headers.contains("access-control-request-headers: x-test"),
      "missing Access-Control-Request-Headers header:\n{preflight_headers}"
    );
    assert!(preflight_body.is_empty(), "expected empty preflight body");

    let actual_headers = captured[1].0.to_ascii_lowercase();
    assert!(
      actual_headers.starts_with("put /preflight"),
      "unexpected actual request:\n{actual_headers}"
    );
    assert!(
      actual_headers.contains("x-test: hello"),
      "missing user header on actual request:\n{actual_headers}"
    );
    assert_eq!(&captured[1].1, b"payload");
  }

  #[test]
  fn cors_preflight_rejects_redirect_and_does_not_send_actual_request() {
    if skip_if_curl_backend_missing(
      "cors_preflight_rejects_redirect_and_does_not_send_actual_request",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_rejects_redirect_and_does_not_send_actual_request")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("options /redir"),
        "expected preflight OPTIONS request line, got:\n{req}"
      );
      let response = "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Ensure no follow-up request arrives (no redirect-follow, no actual request).
      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(200) {
        match listener.accept() {
          Ok(_) => panic!("unexpected follow-up request after preflight redirect"),
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept after redirect: {err}"),
        }
      }
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/redir");
    let mut request = Request::new("PUT", &url);
    request.set_mode(RequestMode::Cors);
    // Add a non-safelisted header so the request is definitely non-simple.
    request.headers.append("X-Test", "1").unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err =
      execute_web_fetch(&fetcher, &request, ctx).expect_err("expected preflight redirect error");
    let message = err.to_string().to_ascii_lowercase();
    assert!(
      message.contains("preflight") && message.contains("redirect"),
      "unexpected error: {err}"
    );

    handle.join().unwrap();
  }

  #[test]
  fn cors_preflight_sends_access_control_request_headers() {
    if skip_if_curl_backend_missing("cors_preflight_sends_access_control_request_headers") {
      return;
    }
    let Some(listener) = try_bind_localhost("cors_preflight_sends_access_control_request_headers")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // Preflight request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      *captured_req.lock().unwrap() = headers;
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: https://client.example\r\n",
        "Access-Control-Allow-Methods: PUT\r\n",
        "Access-Control-Allow-Headers: x-test\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("put /sendhdrs"),
        "expected PUT request line, got:\n{req}"
      );
      assert_eq!(body, b"payload");
      let response_body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: https://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(response_body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/sendhdrs");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let mut request = Request::new("PUT", &url);
    request.set_mode(RequestMode::Cors);
    request.headers.append("X-Test", "hello").unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let ctx = WebFetchExecutionContext {
      destination: FetchDestination::StyleCors,
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();

    let preflight = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      preflight.starts_with("options /sendhdrs"),
      "expected OPTIONS request line, got:\n{preflight}"
    );
    assert!(
      preflight.contains("access-control-request-method: put"),
      "missing Access-Control-Request-Method header:\n{preflight}"
    );
    assert!(
      preflight.contains("access-control-request-headers: x-test"),
      "missing Access-Control-Request-Headers header:\n{preflight}"
    );
    assert!(
      preflight.contains("accept: */*"),
      "missing Accept header:\n{preflight}"
    );
  }

  #[test]
  fn cors_preflight_access_control_request_headers_is_sorted_and_deduped() {
    if skip_if_curl_backend_missing(
      "cors_preflight_access_control_request_headers_is_sorted_and_deduped",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_access_control_request_headers_is_sorted_and_deduped")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // Preflight request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      *captured_req.lock().unwrap() = headers;
      let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: http://client.example\r\nAccess-Control-Allow-Methods: PUT\r\nAccess-Control-Allow-Headers: x-a, x-b\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("put /hdrs"),
        "expected PUT request line, got:\n{req}"
      );
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/hdrs");
    let mut request = Request::new("PUT", &url);
    request.set_mode(RequestMode::Cors);
    // Add two unsafe headers out of order (and a duplicate) so the preflight request must sort and
    // de-duplicate the Access-Control-Request-Headers list.
    request.headers.append("X-B", "1").unwrap();
    request.headers.append("X-A", "2").unwrap();
    request.headers.append("X-A", "3").unwrap();
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();

    let preflight = captured.lock().unwrap().to_ascii_lowercase();
    let mut lines = preflight
      .lines()
      .map(|line| line.trim_end_matches('\r'))
      .filter(|line| line.starts_with("access-control-request-headers:"))
      .collect::<Vec<_>>();
    assert_eq!(
      lines.len(),
      1,
      "expected exactly one Access-Control-Request-Headers header, got:\n{preflight}"
    );
    assert_eq!(
      lines.pop().unwrap(),
      "access-control-request-headers: x-a,x-b"
    );
  }

  #[test]
  fn cors_preflight_omits_access_control_request_headers_when_empty() {
    if skip_if_curl_backend_missing(
      "cors_preflight_omits_access_control_request_headers_when_empty",
    ) {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_omits_access_control_request_headers_when_empty")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // Preflight request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      *captured_req.lock().unwrap() = headers;
      let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: http://client.example\r\nAccess-Control-Allow-Methods: PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let req = headers.to_ascii_lowercase();
      assert!(
        req.starts_with("put /methodonly"),
        "expected PUT request line, got:\n{req}"
      );
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/methodonly");
    let mut request = Request::new("PUT", &url);
    request.set_mode(RequestMode::Cors);
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle.join().unwrap();
    let preflight = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      !preflight.contains("access-control-request-headers:"),
      "unexpected Access-Control-Request-Headers in preflight request:\n{preflight}"
    );
  }

  #[test]
  fn cors_preflight_triggered_by_unsafelisted_content_type() {
    if skip_if_curl_backend_missing("cors_preflight_triggered_by_unsafelisted_content_type") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("cors_preflight_triggered_by_unsafelisted_content_type")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // Preflight request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
      let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: http://client.example\r\n",
        "Access-Control-Allow-Methods: POST\r\n",
        "Access-Control-Allow-Headers: content-type\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n",
        "\r\n",
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
      let body_bytes = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: http://client.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body_bytes).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr}/preflight");
    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(b"payload".to_vec()).unwrap());
    let origin = origin_from_url("http://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };

    let mut response = execute_web_fetch(&fetcher, &request, ctx).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");

    let preflight_headers = captured[0].0.to_ascii_lowercase();
    assert!(
      preflight_headers.starts_with("options /preflight"),
      "unexpected preflight request:\n{preflight_headers}"
    );
    assert!(
      preflight_headers.contains("access-control-request-method: post"),
      "missing Access-Control-Request-Method header:\n{preflight_headers}"
    );
    assert!(
      preflight_headers.contains("access-control-request-headers: content-type"),
      "missing Access-Control-Request-Headers header:\n{preflight_headers}"
    );

    let actual_headers = captured[1].0.to_ascii_lowercase();
    assert!(
      actual_headers.starts_with("post /preflight"),
      "unexpected actual request:\n{actual_headers}"
    );
    assert!(
      actual_headers.contains("content-type: application/json"),
      "missing Content-Type header on actual request:\n{actual_headers}"
    );
    assert_eq!(&captured[1].1, b"payload");
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
    assert!(
      response.url.ends_with("/final"),
      "unexpected url: {}",
      response.url
    );
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"done"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");
    let first = captured[0].to_ascii_lowercase();
    let second = captured[1].to_ascii_lowercase();
    assert!(first.starts_with("get /start"), "first request: {first}");
    assert!(second.starts_with("get /final"), "second request: {second}");
  }

  #[test]
  fn redirect_post_to_get_strips_content_type() {
    if skip_if_curl_backend_missing("redirect_post_to_get_strips_content_type") {
      return;
    }
    let Some(listener) = try_bind_localhost("redirect_post_to_get_strips_content_type") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // First request: redirect.
      let mut stream = accept_http_stream(&listener, "redirect_post_to_get_strips_content_type");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
      let response =
        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Second request: final response.
      let mut stream = accept_http_stream(&listener, "redirect_post_to_get_strips_content_type");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push((headers, body));
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
    let mut request = Request::new("POST", &url);
    request
      .headers
      .append("Content-Type", "application/json")
      .unwrap();
    request.body = Some(Body::new(br#"{"hello":"world"}"#.to_vec()).unwrap());
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert!(response.redirected);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"done"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");
    let first_headers = captured[0].0.to_ascii_lowercase();
    let second_headers = captured[1].0.to_ascii_lowercase();
    assert!(
      first_headers.starts_with("post /start"),
      "first request: {first_headers}"
    );
    assert!(
      first_headers.contains("content-type: application/json"),
      "first request missing content-type, got:\n{first_headers}"
    );
    assert_eq!(&captured[0].1, br#"{"hello":"world"}"#);
    assert!(
      second_headers.starts_with("get /final"),
      "second request: {second_headers}"
    );
    assert!(
      !second_headers.contains("content-type:"),
      "expected redirected GET to drop content-type, got:\n{second_headers}"
    );
    assert!(
      captured[1].1.is_empty(),
      "expected redirected GET to send no body, got {:?}",
      captured[1].1
    );
  }

  #[test]
  fn redirect_cross_origin_strips_authorization() {
    if skip_if_curl_backend_missing("redirect_cross_origin_strips_authorization") {
      return;
    }
    let Some(listener1) = try_bind_localhost("redirect_cross_origin_strips_authorization") else {
      return;
    };
    let Some(listener2) = try_bind_localhost("redirect_cross_origin_strips_authorization") else {
      return;
    };
    let addr1 = listener1.local_addr().unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      // First request: redirect to a different origin (port).
      let mut stream = accept_http_stream(&listener1, "redirect_cross_origin_strips_authorization");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push(headers);
      let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{addr2}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Second request: final response on the other listener.
      let mut stream = accept_http_stream(&listener2, "redirect_cross_origin_strips_authorization");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      captured_req.lock().unwrap().push(headers);
      let body = b"ok";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr1}/start");
    let mut request = Request::new("GET", &url);
    request
      .headers
      .append("Authorization", "Bearer secret")
      .unwrap();
    let mut response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.status, 200);
    assert!(response.redirected);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );
    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two requests, got {captured:?}");
    let first = captured[0].to_ascii_lowercase();
    let second = captured[1].to_ascii_lowercase();
    assert!(first.starts_with("get /start"), "first request: {first}");
    assert!(
      first.contains("authorization: bearer secret"),
      "expected first request to include authorization, got:\n{first}"
    );
    assert!(second.starts_with("get /final"), "second request: {second}");
    assert!(
      !second.contains("authorization:"),
      "expected cross-origin redirect to drop authorization, got:\n{second}"
    );
  }

  #[test]
  fn cors_preflight_runs_for_cross_origin_redirect_destination() {
    if skip_if_curl_backend_missing("cors_preflight_runs_for_cross_origin_redirect_destination") {
      return;
    }
    let Some(listener1) =
      try_bind_localhost("cors_preflight_runs_for_cross_origin_redirect_destination")
    else {
      return;
    };
    let Some(listener2) =
      try_bind_localhost("cors_preflight_runs_for_cross_origin_redirect_destination")
    else {
      return;
    };
    let addr1 = listener1.local_addr().unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let client_origin_header_value = format!("http://{addr1}");

    let handle1 = thread::spawn(move || {
      let mut stream = accept_http_stream(
        &listener1,
        "cors_preflight_runs_for_cross_origin_redirect_destination",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("put /start"),
        "unexpected first request:\n{headers}"
      );
      let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{addr2}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
    });

    let client_origin_for_thread = client_origin_header_value.clone();
    let handle2 = thread::spawn(move || {
      // Preflight request to the redirect destination.
      let mut stream = accept_http_stream(
        &listener2,
        "cors_preflight_runs_for_cross_origin_redirect_destination",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options /final"),
        "expected preflight OPTIONS request, got:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-method: put"),
        "missing Access-Control-Request-Method header:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-headers: x-test"),
        "missing Access-Control-Request-Headers header:\n{headers}"
      );
      let response = format!(
        concat!(
          "HTTP/1.1 204 No Content\r\n",
          "Access-Control-Allow-Origin: {client_origin}\r\n",
          "Access-Control-Allow-Methods: PUT\r\n",
          "Access-Control-Allow-Headers: x-test\r\n",
          "Content-Length: 0\r\n",
          "Connection: close\r\n",
          "\r\n"
        ),
        client_origin = client_origin_for_thread
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request to the redirect destination.
      let mut stream = accept_http_stream(
        &listener2,
        "cors_preflight_runs_for_cross_origin_redirect_destination",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("put /final"),
        "expected PUT request after preflight, got:\n{headers}"
      );
      let body = b"ok";
      let response = format!(
        concat!(
          "HTTP/1.1 200 OK\r\n",
          "Access-Control-Allow-Origin: {client_origin}\r\n",
          "Content-Type: text/plain\r\n",
          "Content-Length: {}\r\n",
          "Connection: close\r\n",
          "\r\n"
        ),
        body.len(),
        client_origin = client_origin_for_thread
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr1}/start");
    let client_origin = origin_from_url(&format!("http://{addr1}/")).expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&client_origin),
      ..WebFetchExecutionContext::default()
    };
    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request.headers.append("X-Test", "hello").unwrap();
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle1.join().unwrap();
    handle2.join().unwrap();
  }

  #[test]
  fn cors_preflight_redirect_does_not_request_suppressed_authorization() {
    if skip_if_curl_backend_missing(
      "cors_preflight_redirect_does_not_request_suppressed_authorization",
    ) {
      return;
    }
    let Some(listener1) =
      try_bind_localhost("cors_preflight_redirect_does_not_request_suppressed_authorization")
    else {
      return;
    };
    let Some(listener2) =
      try_bind_localhost("cors_preflight_redirect_does_not_request_suppressed_authorization")
    else {
      return;
    };
    let addr1 = listener1.local_addr().unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let client_origin_header_value = format!("http://{addr1}");

    let handle1 = thread::spawn(move || {
      let mut stream = accept_http_stream(
        &listener1,
        "cors_preflight_redirect_does_not_request_suppressed_authorization",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("put /start"),
        "unexpected first request:\n{headers}"
      );
      assert!(
        lower.contains("authorization: bearer secret"),
        "expected initial same-origin request to include authorization, got:\n{headers}"
      );
      let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{addr2}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
      );
      stream.write_all(response.as_bytes()).unwrap();
    });

    let client_origin_for_thread = client_origin_header_value.clone();
    let handle2 = thread::spawn(move || {
      // Preflight request to the redirect destination.
      let mut stream = accept_http_stream(
        &listener2,
        "cors_preflight_redirect_does_not_request_suppressed_authorization",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("options /final"),
        "expected preflight OPTIONS request, got:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-method: put"),
        "missing Access-Control-Request-Method header:\n{headers}"
      );
      assert!(
        lower.contains("access-control-request-headers: x-test"),
        "missing Access-Control-Request-Headers header:\n{headers}"
      );
      assert!(
        !lower.contains("authorization"),
        "expected redirect-suppressed Authorization header to be omitted from preflight, got:\n{headers}"
      );
      let response = format!(
        concat!(
          "HTTP/1.1 204 No Content\r\n",
          "Access-Control-Allow-Origin: {client_origin}\r\n",
          "Access-Control-Allow-Methods: PUT\r\n",
          "Access-Control-Allow-Headers: x-test\r\n",
          "Content-Length: 0\r\n",
          "Connection: close\r\n",
          "\r\n"
        ),
        client_origin = client_origin_for_thread
      );
      stream.write_all(response.as_bytes()).unwrap();
      drop(stream);

      // Actual request must not include Authorization after cross-origin redirect.
      let mut stream = accept_http_stream(
        &listener2,
        "cors_preflight_redirect_does_not_request_suppressed_authorization",
      );
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let (headers, _body) = read_http_request(&mut stream);
      let lower = headers.to_ascii_lowercase();
      assert!(
        lower.starts_with("put /final"),
        "expected PUT request after preflight, got:\n{headers}"
      );
      assert!(
        !lower.contains("authorization:"),
        "expected redirected PUT to omit Authorization header, got:\n{headers}"
      );
      let body = b"ok";
      let response = format!(
        concat!(
          "HTTP/1.1 200 OK\r\n",
          "Access-Control-Allow-Origin: {client_origin}\r\n",
          "Content-Type: text/plain\r\n",
          "Content-Length: {}\r\n",
          "Connection: close\r\n",
          "\r\n"
        ),
        body.len(),
        client_origin = client_origin_for_thread
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = test_http_fetcher();
    let url = format!("http://{addr1}/start");
    let client_origin = origin_from_url(&format!("http://{addr1}/")).expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&client_origin),
      ..WebFetchExecutionContext::default()
    };
    let mut request = Request::new("PUT", &url);
    request.credentials = RequestCredentials::Omit;
    request
      .headers
      .append("Authorization", "Bearer secret")
      .unwrap();
    request.headers.append("X-Test", "hello").unwrap();
    let mut response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.status, 200);
    assert_eq!(
      response.body.as_mut().unwrap().consume_bytes().unwrap(),
      b"ok"
    );

    handle1.join().unwrap();
    handle2.join().unwrap();
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
    assert!(matches!(err, Error::Resource(_) | Error::Other(_)));
    handle.join().unwrap();
  }

  #[test]
  fn redirect_manual_returns_opaque_redirect_without_following() {
    if skip_if_curl_backend_missing("redirect_manual_returns_opaque_redirect_without_following") {
      return;
    }
    let Some(listener) =
      try_bind_localhost("redirect_manual_returns_opaque_redirect_without_following")
    else {
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
    let response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).unwrap();
    assert_eq!(response.r#type, ResponseType::OpaqueRedirect);
    assert_eq!(response.status, 0);
    assert_eq!(response.status_text, "");
    assert_eq!(response.url, "");
    assert!(!response.redirected);
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert!(response.headers.sort_and_combine().is_empty());
    assert!(response.body.is_none());
    handle.join().unwrap();
  }

  #[test]
  fn opaque_redirect_does_not_leak_redirected_flag() {
    struct ManualRedirectFetcher;

    impl ResourceFetcher for ManualRedirectFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request")
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.method, "GET");
        assert_eq!(req.redirect, RequestRedirect::Manual);
        assert!(req.headers.is_empty());
        assert!(req.body.is_none());
        let mut resource = FetchedResource::new(b"ok".to_vec(), None);
        resource.status = Some(302);
        // Simulate a redirect that would otherwise be detectable via `final_url`.
        resource.final_url = Some("https://example.com/final".to_string());
        Ok(resource)
      }
    }

    let fetcher = ManualRedirectFetcher;
    let mut request = Request::new("GET", "https://example.com/start");
    request.redirect = RequestRedirect::Manual;
    let response =
      execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default()).expect("response");

    assert_eq!(response.r#type, ResponseType::OpaqueRedirect);
    assert_eq!(response.status, 0);
    assert_eq!(response.url, "");
    assert!(!response.redirected);
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert!(response.headers.sort_and_combine().is_empty());
    assert!(response.body.is_none());
  }

  #[test]
  fn same_origin_blocks_cross_origin_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "https://other.example/res");
    request.set_mode(RequestMode::SameOrigin);
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
  fn same_origin_blocks_cross_origin_final_url_after_redirect() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingFetcher {
      hits: AtomicUsize,
      resource: FetchedResource,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(self.resource.clone())
      }

      fn fetch_with_request(&self, _req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://other.example/res".to_string());
    let fetcher = CountingFetcher {
      hits: AtomicUsize::new(0),
      resource,
    };

    let mut request = Request::new("GET", "https://client.example/res");
    request.set_mode(RequestMode::SameOrigin);
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected error");
    assert_eq!(fetcher.hits.load(Ordering::SeqCst), 1);
    assert!(matches!(err, Error::Other(_)));
    let message = err.to_string();
    assert!(message.contains("same-origin mode"));
    assert!(message.contains("blocked cross-origin"));
    assert!(message.contains("other.example"));
  }

  #[test]
  fn web_fetch_resolves_relative_url_against_referrer_url() {
    struct UrlAssertingFetcher;

    impl ResourceFetcher for UrlAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.url, "https://example.com/dir/sub");
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        panic!("fetch_http_request should not be called for cacheable GET requests");
      }
    }

    let fetcher = UrlAssertingFetcher;
    let mut request = Request::new("GET", "sub");
    request.set_mode(RequestMode::NoCors);
    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://example.com/dir/page"),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Opaque);
    assert_eq!(response.url, "");
  }

  #[test]
  fn web_fetch_resolves_relative_url_against_tolerant_referrer_url() {
    struct UrlAssertingFetcher;

    impl ResourceFetcher for UrlAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.url, "https://example.com/dir/sub");
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        panic!("fetch_http_request should not be called for cacheable GET requests");
      }
    }

    // The referrer URL contains characters (`|`) that `url::Url::parse` rejects, but browsers
    // percent-encode them during URL resolution. We normalize such referrers so relative request
    // URLs still resolve against the full path instead of falling back to the origin root.
    let fetcher = UrlAssertingFetcher;
    let mut request = Request::new("GET", "sub");
    request.set_mode(RequestMode::SameOrigin);
    let ctx = WebFetchExecutionContext {
      referrer_url: Some("https://example.com/dir/page?x=1|2"),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Basic);
    assert_eq!(response.url, "https://example.com/dir/sub");
  }

  #[test]
  fn web_fetch_resolves_relative_url_against_client_origin() {
    struct UrlAssertingFetcher;

    impl ResourceFetcher for UrlAssertingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        assert_eq!(req.url, "https://client.example/res");
        Ok(FetchedResource::new(b"ok".to_vec(), None))
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        panic!("fetch_http_request should not be called for cacheable GET requests");
      }
    }

    let origin = origin_from_url("https://client.example/").expect("origin");
    let fetcher = UrlAssertingFetcher;
    let mut request = Request::new("GET", "/res");
    request.set_mode(RequestMode::NoCors);
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected response");
    assert_eq!(response.r#type, ResponseType::Opaque);
    assert_eq!(response.url, "");
  }

  #[test]
  fn web_fetch_relative_url_without_base_errors_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("GET", "/res");
    request.set_mode(RequestMode::NoCors);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected error");
    assert!(matches!(err, Error::Other(_)));
    assert!(err.to_string().contains("missing base URL"));
  }

  #[test]
  fn csp_connect_src_self_blocks_cross_origin_before_fetching() {
    let fetcher = PanicFetcher;
    let request = Request::new("GET", "https://other.example/res");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let csp = CspPolicy::from_values(["connect-src 'self'"]).expect("csp");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      csp: Some(&csp),
      ..WebFetchExecutionContext::default()
    };

    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected CSP block");
    assert!(matches!(err, Error::Other(_)));
    let message = err.to_string();
    assert!(message.contains("Content-Security-Policy"));
    assert!(message.contains("connect-src"));
    assert!(message.contains("requested URL"));
  }

  #[test]
  fn csp_connect_src_self_blocks_cross_origin_final_url_after_redirect() {
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://other.example/res".to_string());
    // Allow the cross-origin redirect through CORS so we can assert CSP blocks the final URL.
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    let fetcher = StaticFetcher { resource };
    let request = Request::new("GET", "https://client.example/res");
    let origin = origin_from_url("https://client.example/").expect("origin");
    let csp = CspPolicy::from_values(["connect-src 'self'"]).expect("csp");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      csp: Some(&csp),
      ..WebFetchExecutionContext::default()
    };

    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected CSP block");
    assert!(matches!(err, Error::Other(_)));
    let message = err.to_string();
    assert!(message.contains("Content-Security-Policy"));
    assert!(message.contains("connect-src"));
    assert!(message.contains("final URL"));
    assert!(message.contains("other.example"));
  }

  #[test]
  fn csp_connect_src_self_blocks_cross_origin_final_url_for_redirect_manual() {
    struct FollowedRedirectFetcher {
      resource: FetchedResource,
    }

    impl ResourceFetcher for FollowedRedirectFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request")
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://other.example/res".to_string());
    let fetcher = FollowedRedirectFetcher { resource };

    let mut request = Request::new("GET", "https://client.example/res");
    request.redirect = RequestRedirect::Manual;
    let origin = origin_from_url("https://client.example/").expect("origin");
    let csp = CspPolicy::from_values(["connect-src 'self'"]).expect("csp");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      csp: Some(&csp),
      ..WebFetchExecutionContext::default()
    };

    let err = execute_web_fetch(&fetcher, &request, ctx).expect_err("expected CSP block");
    assert!(matches!(err, Error::Other(_)));
    let message = err.to_string();
    assert!(message.contains("Content-Security-Policy"));
    assert!(message.contains("connect-src"));
    assert!(message.contains("final URL"));
    assert!(message.contains("other.example"));
  }

  #[test]
  fn no_cors_rejects_non_safelisted_method_before_fetching() {
    let fetcher = PanicFetcher;
    let mut request = Request::new("PUT", "https://example.com/res");
    request.set_mode(RequestMode::NoCors);
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected no-cors method error");
    assert!(matches!(err, Error::Other(_)));
    let message = err.to_string();
    assert!(message.contains("no-cors"), "unexpected error: {message}");
    assert!(
      message.contains("CORS-safelisted"),
      "unexpected error: {message}"
    );
  }

  #[test]
  fn no_cors_skips_cors_validation_and_returns_opaque() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingFetcher {
      hits: AtomicUsize,
      resource: FetchedResource,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(self.resource.clone())
      }

      fn fetch_with_request(&self, _req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(self.resource.clone())
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(self.resource.clone())
      }
    }

    // Ensure `Response.redirected` does not leak redirect information for opaque responses.
    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://other.example/final".to_string());
    let fetcher = CountingFetcher {
      hits: AtomicUsize::new(0),
      resource,
    };
    let mut request = Request::new("GET", "https://other.example/res");
    request.set_mode(RequestMode::NoCors);
    let origin = origin_from_url("https://client.example/").expect("origin");
    let ctx = WebFetchExecutionContext {
      client_origin: Some(&origin),
      ..WebFetchExecutionContext::default()
    };
    let response = execute_web_fetch(&fetcher, &request, ctx).expect("expected opaque response");
    assert_eq!(fetcher.hits.load(Ordering::SeqCst), 1);
    assert_eq!(response.r#type, ResponseType::Opaque);
    assert_eq!(response.status, 0);
    assert_eq!(response.status_text, "");
    assert_eq!(response.url, "");
    assert!(!response.redirected);
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert!(response.headers.sort_and_combine().is_empty());
    assert!(response.body.is_none());
  }

  #[test]
  fn redirect_error_rejects_when_final_url_differs() {
    struct FollowedRedirectFetcher {
      resource: FetchedResource,
    }

    impl ResourceFetcher for FollowedRedirectFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://example.com/b".to_string());
    resource.status = Some(200);
    let fetcher = FollowedRedirectFetcher { resource };

    let mut request = Request::new("GET", "https://example.com/a");
    request.redirect = RequestRedirect::Error;
    let err = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect_err("expected redirect error");
    assert!(matches!(err, Error::Other(_)));
  }

  #[test]
  fn redirect_manual_returns_opaque_redirect_when_redirected() {
    struct FollowedRedirectFetcher {
      resource: FetchedResource,
    }

    impl ResourceFetcher for FollowedRedirectFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        unreachable!("execute_web_fetch should call fetch_http_request");
      }

      fn fetch_http_request(&self, _req: HttpRequest<'_>) -> Result<FetchedResource> {
        Ok(self.resource.clone())
      }
    }

    let mut resource = FetchedResource::new(b"ok".to_vec(), None);
    resource.final_url = Some("https://example.com/b".to_string());
    resource.status = Some(200);
    let fetcher = FollowedRedirectFetcher { resource };

    let mut request = Request::new("GET", "https://example.com/a");
    request.redirect = RequestRedirect::Manual;
    let response = execute_web_fetch(&fetcher, &request, WebFetchExecutionContext::default())
      .expect("expected response");

    assert_eq!(response.r#type, ResponseType::OpaqueRedirect);
    assert_eq!(response.status, 0);
    assert_eq!(response.status_text, "");
    assert_eq!(response.url, "");
    assert!(!response.redirected);
    assert_eq!(response.headers.guard(), HeadersGuard::Immutable);
    assert!(response.headers.sort_and_combine().is_empty());
    assert!(response.body.is_none());
  }
}
