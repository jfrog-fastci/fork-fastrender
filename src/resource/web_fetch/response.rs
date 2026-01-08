use super::{Body, Headers, HeadersGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseType {
  Basic,
  Cors,
  Default,
  Error,
  Opaque,
  OpaqueRedirect,
}

/// A minimal, spec-shaped response object.
#[derive(Debug)]
pub struct Response {
  pub r#type: ResponseType,
  pub url: String,
  pub redirected: bool,
  pub status: u16,
  pub status_text: String,
  pub headers: Headers,
  pub body: Option<Body>,
}

impl Response {
  pub fn new(status: u16) -> Self {
    Self {
      r#type: ResponseType::Default,
      url: String::new(),
      redirected: false,
      status,
      status_text: String::new(),
      headers: Headers::new_with_guard(HeadersGuard::Response),
      body: None,
    }
  }
}

impl Clone for Response {
  fn clone(&self) -> Self {
    Self {
      r#type: self.r#type,
      url: self.url.clone(),
      redirected: self.redirected,
      status: self.status,
      status_text: self.status_text.clone(),
      headers: self.headers.clone(),
      body: self.body.clone(),
    }
  }
}

