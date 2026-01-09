use thiserror::Error;

/// Deterministic DOM mutation error codes.
///
/// These map 1:1 to Web IDL exception names, and are designed to be thrown as JS
/// `DOMException`s later.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DomError {
  #[error("HierarchyRequestError")]
  HierarchyRequestError,
  #[error("NotFoundError")]
  NotFoundError,
  #[error("InvalidNodeType")]
  InvalidNodeType,
  #[error("SyntaxError")]
  SyntaxError,
}

impl DomError {
  pub fn code(self) -> &'static str {
    match self {
      Self::HierarchyRequestError => "HierarchyRequestError",
      Self::NotFoundError => "NotFoundError",
      Self::InvalidNodeType => "InvalidNodeType",
      Self::SyntaxError => "SyntaxError",
    }
  }
}

pub type Result<T> = std::result::Result<T, DomError>;
