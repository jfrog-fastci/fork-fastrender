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
  #[error("InvalidStateError")]
  InvalidStateError,
  #[error("NamespaceError")]
  NamespaceError,
  #[error("NotFoundError")]
  NotFoundError,
  #[error("NotSupportedError")]
  NotSupportedError,
  #[error("WrongDocumentError")]
  WrongDocumentError,
  /// Legacy alias for [`DomError::InvalidNodeTypeError`].
  ///
  /// Kept to avoid breaking older call sites that referenced `InvalidNodeType` (without the
  /// DOM-standard `Error` suffix).
  #[error("InvalidNodeTypeError")]
  InvalidNodeType,
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
      Self::InvalidStateError => "InvalidStateError",
      Self::NamespaceError => "NamespaceError",
      Self::NotFoundError => "NotFoundError",
      Self::NotSupportedError => "NotSupportedError",
      Self::WrongDocumentError => "WrongDocumentError",
      Self::InvalidNodeType | Self::InvalidNodeTypeError => "InvalidNodeTypeError",
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
  fn dom_error_code_matches_dom_exception_names() {
    assert_eq!(DomError::HierarchyRequestError.code(), "HierarchyRequestError");
    assert_eq!(DomError::IndexSizeError.code(), "IndexSizeError");
    assert_eq!(DomError::InvalidCharacterError.code(), "InvalidCharacterError");
    assert_eq!(DomError::InvalidStateError.code(), "InvalidStateError");
    assert_eq!(DomError::WrongDocumentError.code(), "WrongDocumentError");

    assert_eq!(DomError::InvalidStateError.to_string(), "InvalidStateError");
    assert_eq!(DomError::InvalidNodeTypeError.to_string(), "InvalidNodeTypeError");

    // Ensure both the legacy `InvalidNodeType` and the preferred `InvalidNodeTypeError` map to the
    // spec-correct DOMException name.
    assert_eq!(DomError::InvalidNodeType.code(), "InvalidNodeTypeError");
    assert_eq!(DomError::InvalidNodeTypeError.code(), "InvalidNodeTypeError");
    assert_eq!(DomError::InvalidNodeType.to_string(), "InvalidNodeTypeError");
  }
}
