use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DomError {
  #[error("invalid node type")]
  InvalidNodeType,
  #[error("hierarchy request error")]
  HierarchyRequest,
  #[error("node not found")]
  NotFound,
}

pub type Result<T> = std::result::Result<T, DomError>;
