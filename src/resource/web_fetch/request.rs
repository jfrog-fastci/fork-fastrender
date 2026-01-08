use super::{Body, Headers, HeadersGuard};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRedirect {
  Follow,
  Error,
  Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferrerPolicy {
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

/// A minimal, spec-shaped request object.
#[derive(Debug)]
pub struct Request {
  pub method: String,
  pub url: String,
  pub headers: Headers,
  pub mode: RequestMode,
  pub credentials: RequestCredentials,
  pub redirect: RequestRedirect,
  pub referrer: String,
  pub referrer_policy: ReferrerPolicy,
  pub body: Option<Body>,
}

impl Request {
  pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
    let mode = RequestMode::Cors;
    let headers_guard = match mode {
      RequestMode::NoCors => HeadersGuard::RequestNoCors,
      _ => HeadersGuard::Request,
    };

    Self {
      method: method.into(),
      url: url.into(),
      headers: Headers::new_with_guard(headers_guard),
      mode,
      credentials: RequestCredentials::SameOrigin,
      redirect: RequestRedirect::Follow,
      referrer: String::new(),
      referrer_policy: ReferrerPolicy::EmptyString,
      body: None,
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

