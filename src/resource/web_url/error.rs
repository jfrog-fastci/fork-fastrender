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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebUrlSetter {
  Href,
  Protocol,
  Username,
  Password,
  Host,
  Hostname,
  Port,
  Pathname,
  Search,
  Hash,
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
  InvalidBase {
    base: String,
    source: ::url::ParseError,
  },
  Parse {
    input: String,
    base: Option<String>,
    source: ::url::ParseError,
  },
  SetterFailure {
    setter: WebUrlSetter,
    value: String,
    source: Option<::url::ParseError>,
  },
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
      WebUrlError::InvalidBase { base, source } => {
        write!(f, "invalid base URL {base:?}: {source}")
      }
      WebUrlError::Parse { input, base, source } => {
        if let Some(base) = base {
          write!(f, "failed to parse URL {input:?} with base {base:?}: {source}")
        } else {
          write!(f, "failed to parse URL {input:?}: {source}")
        }
      }
      WebUrlError::SetterFailure {
        setter,
        value,
        source,
      } => {
        if let Some(source) = source {
          write!(f, "failed to set {setter:?} to {value:?}: {source}")
        } else {
          write!(f, "failed to set {setter:?} to {value:?}")
        }
      }
    }
  }
}

impl std::error::Error for WebUrlError {}
