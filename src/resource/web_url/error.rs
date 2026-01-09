use std::collections::TryReserveError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebUrlLimitKind {
  /// Limit for URL / query input string lengths.
  InputBytes,
  /// Limit for number of query pairs in a `URLSearchParams` list.
  QueryPairs,
  /// Limit for total decoded name/value bytes across a `URLSearchParams` list.
  TotalQueryBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebUrlError {
  OutOfMemory,
  /// The URL (or base URL) could not be parsed.
  ParseError,
  LimitExceeded {
    kind: WebUrlLimitKind,
    limit: usize,
    attempted: usize,
  },
  InvalidUtf8,
}

impl From<TryReserveError> for WebUrlError {
  fn from(_: TryReserveError) -> Self {
    Self::OutOfMemory
  }
}

impl std::fmt::Display for WebUrlError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      WebUrlError::OutOfMemory => write!(f, "out of memory"),
      WebUrlError::ParseError => write!(f, "failed to parse URL"),
      WebUrlError::InvalidUtf8 => write!(f, "invalid UTF-8"),
      WebUrlError::LimitExceeded {
        kind,
        limit,
        attempted,
      } => write!(
        f,
        "limit exceeded ({kind:?}): attempted {attempted} bytes/items (limit {limit})"
      ),
    }
  }
}

impl std::error::Error for WebUrlError {}
