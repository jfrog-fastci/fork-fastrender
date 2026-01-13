use super::{Body, Headers, HeadersGuard, WebFetchLimits};
use crate::resource::{FetchCredentialsMode, ReferrerPolicy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
  Navigate,
  SameOrigin,
  NoCors,
  Cors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCredentials {
  Omit,
  SameOrigin,
  Include,
}

impl From<RequestCredentials> for FetchCredentialsMode {
  fn from(value: RequestCredentials) -> Self {
    match value {
      RequestCredentials::Omit => Self::Omit,
      RequestCredentials::SameOrigin => Self::SameOrigin,
      RequestCredentials::Include => Self::Include,
    }
  }
}

impl From<FetchCredentialsMode> for RequestCredentials {
  fn from(value: FetchCredentialsMode) -> Self {
    match value {
      FetchCredentialsMode::Omit => Self::Omit,
      FetchCredentialsMode::SameOrigin => Self::SameOrigin,
      FetchCredentialsMode::Include => Self::Include,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestRedirect {
  Follow,
  Error,
  Manual,
}

/// A minimal, spec-shaped request object.
#[derive(Debug)]
pub struct Request {
  pub method: String,
  pub url: String,
  pub headers: Headers,
  pub mode: RequestMode,
  pub credentials: RequestCredentials,
  pub redirect: RequestRedirect,
  /// Referrer for this request.
  ///
  /// Semantics used by `web_fetch::execute_web_fetch`:
  /// - `""` (empty) means "use the execution context's default referrer".
  /// - `"no-referrer"` is treated as a sentinel meaning "explicitly omit the referrer".
  /// - Any other value is treated as a URL string (the adapter does not validate it).
  pub referrer: String,
  /// Referrer policy override for this request.
  ///
  /// [`ReferrerPolicy::EmptyString`] represents the empty-string state from the spec and means
  /// "use the execution context's default referrer policy".
  pub referrer_policy: ReferrerPolicy,
  pub body: Option<Body>,
}

impl Request {
  pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
    Self::new_with_limits(method, url, &WebFetchLimits::default())
  }

  pub fn new_with_limits(
    method: impl Into<String>,
    url: impl Into<String>,
    limits: &WebFetchLimits,
  ) -> Self {
    let mut request = Self {
      method: method.into(),
      url: url.into(),
      headers: Headers::new_with_guard_and_limits(HeadersGuard::Request, limits),
      mode: RequestMode::Cors,
      credentials: RequestCredentials::SameOrigin,
      redirect: RequestRedirect::Follow,
      referrer: String::new(),
      referrer_policy: ReferrerPolicy::EmptyString,
      body: None,
    };
    request.set_mode(RequestMode::Cors);
    request
  }

  pub fn set_mode(&mut self, mode: RequestMode) {
    self.mode = mode;
    let guard = match mode {
      RequestMode::NoCors => HeadersGuard::RequestNoCors,
      _ => HeadersGuard::Request,
    };

    // Fetch never mutates a request's mode after construction, but FastRender's core request type
    // exposes `set_mode()` for convenience when adapting non-spec call sites (e.g. JS bindings).
    //
    // When switching to `no-cors`, we must re-apply the existing header list under the stricter
    // `request-no-cors` guard so non-safelisted headers are removed deterministically.
    if self.headers.guard() == guard {
      return;
    }

    if guard == HeadersGuard::RequestNoCors {
      let existing = self.headers.raw_pairs();
      let limits = self.headers.limits().clone();
      let mut headers = Headers::new_with_guard_and_limits(guard, &limits);
      if headers.fill_from_pairs(existing).is_ok() {
        self.headers = headers;
      } else {
        // `existing` comes from a previously-validated header list and should always be valid.
        // Avoid panicking in production; fall back to an empty list under the new guard.
        self.headers = Headers::new_with_guard_and_limits(guard, &limits);
      }
    } else {
      self.headers.set_guard(guard);
    }
  }
}

impl Clone for Request {
  fn clone(&self) -> Self {
    Self {
      method: self.method.clone(),
      url: self.url.clone(),
      headers: self.headers.clone(),
      mode: self.mode,
      credentials: self.credentials,
      redirect: self.redirect,
      referrer: self.referrer.clone(),
      referrer_policy: self.referrer_policy,
      body: self.body.clone(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn set_mode_to_no_cors_refilters_headers() {
    let mut req = Request::new("GET", "https://example.com/");
    req.headers.append("Accept", "text/plain").unwrap();
    req.headers.append("x-test", "ok").unwrap();
    req.headers.append("Range", "bytes=0-1").unwrap();

    assert!(req.headers.has("accept").unwrap());
    assert!(req.headers.has("x-test").unwrap());
    assert!(req.headers.has("range").unwrap());

    req.set_mode(RequestMode::NoCors);

    assert_eq!(req.headers.guard(), HeadersGuard::RequestNoCors);
    assert!(req.headers.has("accept").unwrap());
    assert!(!req.headers.has("x-test").unwrap());
    assert!(!req.headers.has("range").unwrap());
  }
}
