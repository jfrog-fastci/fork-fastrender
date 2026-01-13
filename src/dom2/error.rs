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
  #[error("InvalidNodeTypeError")]
  InvalidNodeTypeError,
  #[error("NoModificationAllowedError")]
  NoModificationAllowedError,
  #[error("SyntaxError")]
  SyntaxError,
  #[error("WrongDocumentError")]
  WrongDocumentError,
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
      Self::InvalidNodeTypeError => "InvalidNodeTypeError",
      Self::NoModificationAllowedError => "NoModificationAllowedError",
      Self::SyntaxError => "SyntaxError",
      Self::WrongDocumentError => "WrongDocumentError",
    }
  }
}

pub type Result<T> = std::result::Result<T, DomError>;
