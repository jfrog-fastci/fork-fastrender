use super::{Result, WebFetchError, WebFetchLimitKind, WebFetchLimits};

/// An in-memory Fetch Body implementation.
///
/// This is intentionally *not* a streaming model yet; it only stores bytes and tracks whether the
/// body has been consumed.
#[derive(Debug, Default)]
pub struct Body {
  bytes: Vec<u8>,
  body_used: bool,
}

impl Body {
  pub fn new(bytes: Vec<u8>) -> Result<Self> {
    Self::new_with_limits(bytes, &WebFetchLimits::default())
  }

  pub fn new_with_limits(bytes: Vec<u8>, limits: &WebFetchLimits) -> Result<Self> {
    let len = bytes.len();
    if len > limits.max_request_body_bytes {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::RequestBodyBytes,
        limit: limits.max_request_body_bytes,
        attempted: len,
      });
    }
    Ok(Self {
      bytes,
      body_used: false,
    })
  }

  pub fn empty() -> Self {
    Self {
      bytes: Vec::new(),
      body_used: false,
    }
  }

  pub(crate) fn new_response(bytes: Vec<u8>, limits: &WebFetchLimits) -> Result<Self> {
    let len = bytes.len();
    if len > limits.max_response_body_bytes {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::ResponseBodyBytes,
        limit: limits.max_response_body_bytes,
        attempted: len,
      });
    }
    Ok(Self {
      bytes,
      body_used: false,
    })
  }

  /// Return the underlying bytes without consuming the body.
  ///
  /// In this in-memory model, `execute_web_fetch()` can send the request body without marking it
  /// as used; consumption only happens when the JavaScript-visible body is read (`text()`, `json()`,
  /// etc).
  pub fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }

  pub fn bytes(&self) -> &[u8] {
    self.as_bytes()
  }

  pub fn body_used(&self) -> bool {
    self.body_used
  }

  /// Consume the body as bytes.
  ///
  /// Subsequent consumption attempts return [`WebFetchError::BodyUsed`].
  pub fn consume_bytes(&mut self) -> Result<Vec<u8>> {
    if self.body_used {
      return Err(WebFetchError::BodyUsed);
    }
    self.body_used = true;
    Ok(self.bytes.clone())
  }

  /// Consume the body as UTF-8 text.
  pub fn text_utf8(&mut self) -> Result<String> {
    let bytes = self.consume_bytes()?;
    Ok(String::from_utf8(bytes)?)
  }

  /// Consume the body as JSON.
  pub fn json(&mut self) -> Result<serde_json::Value> {
    let bytes = self.consume_bytes()?;
    Ok(serde_json::from_slice(&bytes)?)
  }
}

impl Clone for Body {
  fn clone(&self) -> Self {
    // Fetch `Body` cloning tees a stream; since this in-memory model has no streaming yet, cloning
    // is a cheap "new unconsumed view over the same bytes".
    Self {
      bytes: self.bytes.clone(),
      body_used: false,
    }
  }
}
