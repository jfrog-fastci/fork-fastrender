use super::{Result, WebFetchError};

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
  pub fn new(bytes: Vec<u8>) -> Self {
    Self {
      bytes,
      body_used: false,
    }
  }

  pub fn empty() -> Self {
    Self::new(Vec::new())
  }

  /// Return the underlying bytes without consuming the body.
  ///
  /// In this in-memory model, `execute_web_fetch()` can send the request body without marking it
  /// as used; consumption only happens when the JavaScript-visible body is read (`text()`, `json()`,
  /// etc).
  pub fn as_bytes(&self) -> &[u8] {
    &self.bytes
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
