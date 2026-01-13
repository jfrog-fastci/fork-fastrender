use thiserror::Error;

/// Deterministic DOM mutation error codes.
///
/// These map 1:1 to Web IDL exception names, and are designed to be thrown as JS
/// `DOMException`s later.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DomError {
  #[error("HierarchyRequestError")]
  HierarchyRequestError,
  #[error("IndexSizeError")]
  IndexSizeError,
  #[error("InvalidCharacterError")]
  InvalidCharacterError,
  #[error("NamespaceError")]
  NamespaceError,
  #[error("NotFoundError")]
  NotFoundError,
  #[error("InvalidStateError")]
  InvalidStateError,
  #[error("NotSupportedError")]
  NotSupportedError,
  #[error("WrongDocumentError")]
  WrongDocumentError,
  #[error("InvalidNodeTypeError")]
  InvalidNodeTypeError,
  #[error("NoModificationAllowedError")]
  NoModificationAllowedError,
  #[error("SyntaxError")]
  SyntaxError,
}

impl DomError {
  pub fn code(self) -> &'static str {
    match self {
      Self::HierarchyRequestError => "HierarchyRequestError",
      Self::IndexSizeError => "IndexSizeError",
      Self::InvalidCharacterError => "InvalidCharacterError",
      Self::NamespaceError => "NamespaceError",
      Self::NotFoundError => "NotFoundError",
      Self::InvalidStateError => "InvalidStateError",
      Self::NotSupportedError => "NotSupportedError",
      Self::WrongDocumentError => "WrongDocumentError",
      Self::InvalidNodeTypeError => "InvalidNodeTypeError",
      Self::NoModificationAllowedError => "NoModificationAllowedError",
      Self::SyntaxError => "SyntaxError",
    }
  }
}

pub type Result<T> = std::result::Result<T, DomError>;

#[cfg(test)]
mod tests {
  use super::DomError;

  #[test]
  fn dom_error_codes_match_dom_exception_names() {
    assert_eq!(DomError::InvalidStateError.code(), "InvalidStateError");
    assert_eq!(DomError::InvalidNodeTypeError.code(), "InvalidNodeTypeError");
    assert_eq!(DomError::InvalidStateError.to_string(), "InvalidStateError");
    assert_eq!(DomError::InvalidNodeTypeError.to_string(), "InvalidNodeTypeError");
  }
}
