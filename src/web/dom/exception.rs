use thiserror::Error;

/// Minimal DOMException representation for Web IDL-ish APIs.
///
/// This is intentionally small for now; new variants should be added as more DOM APIs are exposed
/// to JS.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DomException {
  #[error("SyntaxError: {message}")]
  SyntaxError { message: String },
  #[error("NoModificationAllowedError: {message}")]
  NoModificationAllowedError { message: String },
  #[error("NotSupportedError: {message}")]
  NotSupportedError { message: String },
}

impl DomException {
  pub fn syntax_error(message: impl Into<String>) -> Self {
    Self::SyntaxError {
      message: message.into(),
    }
  }

  pub fn no_modification_allowed_error(message: impl Into<String>) -> Self {
    Self::NoModificationAllowedError {
      message: message.into(),
    }
  }

  pub fn not_supported_error(message: impl Into<String>) -> Self {
    Self::NotSupportedError {
      message: message.into(),
    }
  }
}
