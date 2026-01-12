use thiserror::Error;

/// Deterministic DOM mutation error codes.
///
/// These map 1:1 to Web IDL exception names, and are designed to be thrown as JS
/// `DOMException`s later.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DomError {
  #[error("HierarchyRequestError")]
  HierarchyRequestError,
  #[error("InvalidCharacterError")]
  InvalidCharacterError,
  #[error("NamespaceError")]
  NamespaceError,
  #[error("NotFoundError")]
  NotFoundError,
  #[error("NotSupportedError")]
  NotSupportedError,
  #[error("InvalidNodeType")]
  InvalidNodeType,
  #[error("NoModificationAllowedError")]
  NoModificationAllowedError,
  #[error("SyntaxError")]
  SyntaxError,
}

impl DomError {
  pub fn code(self) -> &'static str {
    match self {
      Self::HierarchyRequestError => "HierarchyRequestError",
      Self::InvalidCharacterError => "InvalidCharacterError",
      Self::NamespaceError => "NamespaceError",
      Self::NotFoundError => "NotFoundError",
      Self::NotSupportedError => "NotSupportedError",
      Self::InvalidNodeType => "InvalidNodeType",
      Self::NoModificationAllowedError => "NoModificationAllowedError",
      Self::SyntaxError => "SyntaxError",
    }
  }
}

pub type Result<T> = std::result::Result<T, DomError>;
