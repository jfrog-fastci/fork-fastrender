use core::fmt;

/// Canonical identifier for a known JavaScript/TypeScript API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiId {
  ArrayPrototypeMap,
  ArrayPrototypeFilter,
  ArrayPrototypeReduce,
  StringPrototypeToLowerCase,
  PromisePrototypeThen,
  MapPrototypeGet,
  JsonParse,
}

impl ApiId {
  pub fn as_str(self) -> &'static str {
    match self {
      ApiId::ArrayPrototypeMap => "Array.prototype.map",
      ApiId::ArrayPrototypeFilter => "Array.prototype.filter",
      ApiId::ArrayPrototypeReduce => "Array.prototype.reduce",
      ApiId::StringPrototypeToLowerCase => "String.prototype.toLowerCase",
      ApiId::PromisePrototypeThen => "Promise.prototype.then",
      ApiId::MapPrototypeGet => "Map.prototype.get",
      ApiId::JsonParse => "JSON.parse",
    }
  }
}

impl fmt::Display for ApiId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

