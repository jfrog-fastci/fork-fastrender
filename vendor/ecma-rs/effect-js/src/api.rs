use core::fmt;

/// Canonical identifier for a known JavaScript/TypeScript API surface.
///
/// This is a small curated set used by early effect analyses and pattern
/// recognition. Each variant maps to a knowledge-base entry via [`ApiId::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiId {
  ArrayPrototypeMap,
  ArrayPrototypeFilter,
  ArrayPrototypeReduce,
  ArrayPrototypeForEach,
  ArrayPrototypeLength,
  StringPrototypeLength,
  StringPrototypeToLowerCase,
  StringPrototypeSplit,
  PromiseAll,
  PromisePrototypeThen,
  MapPrototypeHas,
  MapPrototypeGet,
  Fetch,
  JsonParse,
  MathSqrt,
  ObjectKeys,
  ConsoleLog,
  UrlPrototypeHref,
  UrlPrototypePathname,
  UrlPrototypeOrigin,
  UrlPrototypeProtocol,
  UrlPrototypeHost,
  UrlPrototypeHostname,
  UrlPrototypePort,
  UrlPrototypeSearch,
  UrlPrototypeHash,
}

impl ApiId {
  pub fn as_str(self) -> &'static str {
    match self {
      ApiId::ArrayPrototypeMap => "Array.prototype.map",
      ApiId::ArrayPrototypeFilter => "Array.prototype.filter",
      ApiId::ArrayPrototypeReduce => "Array.prototype.reduce",
      ApiId::ArrayPrototypeForEach => "Array.prototype.forEach",
      ApiId::ArrayPrototypeLength => "Array.prototype.length",
      ApiId::StringPrototypeLength => "String.prototype.length",
      ApiId::StringPrototypeToLowerCase => "String.prototype.toLowerCase",
      ApiId::StringPrototypeSplit => "String.prototype.split",
      ApiId::PromiseAll => "Promise.all",
      ApiId::PromisePrototypeThen => "Promise.prototype.then",
      ApiId::MapPrototypeHas => "Map.prototype.has",
      ApiId::MapPrototypeGet => "Map.prototype.get",
      ApiId::Fetch => "fetch",
      ApiId::JsonParse => "JSON.parse",
      ApiId::MathSqrt => "Math.sqrt",
      ApiId::ObjectKeys => "Object.keys",
      ApiId::ConsoleLog => "console.log",
      ApiId::UrlPrototypeHref => "URL.prototype.href",
      ApiId::UrlPrototypePathname => "URL.prototype.pathname",
      ApiId::UrlPrototypeOrigin => "URL.prototype.origin",
      ApiId::UrlPrototypeProtocol => "URL.prototype.protocol",
      ApiId::UrlPrototypeHost => "URL.prototype.host",
      ApiId::UrlPrototypeHostname => "URL.prototype.hostname",
      ApiId::UrlPrototypePort => "URL.prototype.port",
      ApiId::UrlPrototypeSearch => "URL.prototype.search",
      ApiId::UrlPrototypeHash => "URL.prototype.hash",
    }
  }

  /// Resolve a canonical knowledge-base name (e.g. `"JSON.parse"`) into an [`ApiId`].
  pub fn from_kb_name(name: &str) -> Option<Self> {
    Some(match name {
      "Array.prototype.map" => ApiId::ArrayPrototypeMap,
      "Array.prototype.filter" => ApiId::ArrayPrototypeFilter,
      "Array.prototype.reduce" => ApiId::ArrayPrototypeReduce,
      "Array.prototype.forEach" => ApiId::ArrayPrototypeForEach,
      "Array.prototype.length" => ApiId::ArrayPrototypeLength,
      "String.prototype.length" => ApiId::StringPrototypeLength,
      "String.prototype.toLowerCase" => ApiId::StringPrototypeToLowerCase,
      "String.prototype.split" => ApiId::StringPrototypeSplit,
      "Promise.all" => ApiId::PromiseAll,
      "Promise.prototype.then" => ApiId::PromisePrototypeThen,
      "Map.prototype.get" => ApiId::MapPrototypeGet,
      "Map.prototype.has" => ApiId::MapPrototypeHas,
      "fetch" => ApiId::Fetch,
      "JSON.parse" => ApiId::JsonParse,
      "Math.sqrt" => ApiId::MathSqrt,
      "Object.keys" => ApiId::ObjectKeys,
      "console.log" => ApiId::ConsoleLog,
      "URL.prototype.href" => ApiId::UrlPrototypeHref,
      "URL.prototype.pathname" => ApiId::UrlPrototypePathname,
      "URL.prototype.origin" => ApiId::UrlPrototypeOrigin,
      "URL.prototype.protocol" => ApiId::UrlPrototypeProtocol,
      "URL.prototype.host" => ApiId::UrlPrototypeHost,
      "URL.prototype.hostname" => ApiId::UrlPrototypeHostname,
      "URL.prototype.port" => ApiId::UrlPrototypePort,
      "URL.prototype.search" => ApiId::UrlPrototypeSearch,
      "URL.prototype.hash" => ApiId::UrlPrototypeHash,
      _ => return None,
    })
  }
}

impl fmt::Display for ApiId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}
