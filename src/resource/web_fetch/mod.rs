//! Spec-shaped core types for the WHATWG Fetch platform APIs.
//!
//! This module is intentionally independent from any JavaScript engine. JS bindings should be thin
//! wrappers that hold/own these Rust types and expose WebIDL behavior.

mod body;
mod adapter;
mod headers;
mod request;
mod response;

pub use adapter::{execute_web_fetch, WebFetchExecutionContext};
pub use body::Body;
pub use headers::{Headers, HeadersGuard};
pub use request::{ReferrerPolicy, Request, RequestCredentials, RequestMode, RequestRedirect};
pub use response::{Response, ResponseType};

/// Errors returned by the Fetch core types.
#[derive(Debug, thiserror::Error)]
pub enum WebFetchError {
  #[error("invalid header name: {name:?}")]
  InvalidHeaderName { name: String },

  #[error("invalid header value: {value:?}")]
  InvalidHeaderValue { value: String },

  #[error("headers are immutable")]
  HeadersImmutable,

  #[error("body is already used")]
  BodyUsed,

  #[error("body is not valid UTF-8: {0}")]
  BodyInvalidUtf8(#[from] std::string::FromUtf8Error),

  #[error("body is not valid JSON: {0}")]
  BodyInvalidJson(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, WebFetchError>;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn headers_validation_invalid_name() {
    let mut headers = Headers::new();
    let err = headers.append("bad header", "x").unwrap_err();
    assert!(matches!(err, WebFetchError::InvalidHeaderName { .. }));
  }

  #[test]
  fn headers_validation_invalid_value() {
    let mut headers = Headers::new();
    let err = headers.append("x-test", "hello\nworld").unwrap_err();
    assert!(matches!(err, WebFetchError::InvalidHeaderValue { .. }));
  }

  #[test]
  fn headers_get_combines_values() {
    let mut headers = Headers::new();
    headers.append("X-Test", "a").unwrap();
    headers.append("x-test", "b").unwrap();
    assert_eq!(headers.get("X-TEST").unwrap().as_deref(), Some("a, b"));
  }

  #[test]
  fn headers_set_replaces_first_and_removes_rest() {
    let mut headers = Headers::new();
    headers.append("x-test", "a").unwrap();
    headers.append("x-test", "b").unwrap();
    headers.append("x-test", "c").unwrap();

    headers.set("x-test", "z").unwrap();
    assert_eq!(headers.get("x-test").unwrap().as_deref(), Some("z"));
  }

  #[test]
  fn headers_guard_immutable_throws() {
    let mut headers = Headers::new_with_guard(HeadersGuard::Immutable);
    let err = headers.set("x-test", "a").unwrap_err();
    assert!(matches!(err, WebFetchError::HeadersImmutable));
  }

  #[test]
  fn headers_guard_request_ignores_forbidden_headers() {
    let mut headers = Headers::new_with_guard(HeadersGuard::Request);
    headers.set("cookie", "a=b").unwrap();
    assert!(!headers.has("cookie").unwrap());
  }

  #[test]
  fn headers_guard_response_ignores_forbidden_response_headers() {
    let mut headers = Headers::new_with_guard(HeadersGuard::Response);
    headers.set("set-cookie", "a=b").unwrap();
    assert!(!headers.has("set-cookie").unwrap());
  }

  #[test]
  fn body_consumption_marks_body_used() {
    let mut body = Body::new(b"hello".to_vec());
    assert!(!body.body_used());
    assert_eq!(body.consume_bytes().unwrap(), b"hello".to_vec());
    assert!(body.body_used());

    let err = body.consume_bytes().unwrap_err();
    assert!(matches!(err, WebFetchError::BodyUsed));
  }

  #[test]
  fn body_clone_is_unconsumed() {
    let mut body = Body::new(b"hello".to_vec());
    let _ = body.consume_bytes().unwrap();
    assert!(body.body_used());

    let mut cloned = body.clone();
    assert!(!cloned.body_used());
    assert_eq!(cloned.consume_bytes().unwrap(), b"hello".to_vec());
  }
}
