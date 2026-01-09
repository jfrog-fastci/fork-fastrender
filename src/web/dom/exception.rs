use thiserror::Error;

/// Minimal DOMException representation for Web IDL-ish APIs.
///
/// This is intentionally small for now; new variants should be added as more DOM APIs are exposed
/// to JS.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DomException {
  #[error("SyntaxError: {message}")]
  SyntaxError { message: String },
}

impl DomException {
  pub fn syntax_error(message: impl Into<String>) -> Self {
    Self::SyntaxError {
      message: message.into(),
    }
  }
}
