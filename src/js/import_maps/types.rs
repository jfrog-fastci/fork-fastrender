use std::cmp::Ordering;

use thiserror::Error;
use url::Url;

/// A normalized WHATWG HTML import map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportMap {
  /// Normalized top-level imports.
  pub imports: ModuleSpecifierMap,
  /// Normalized scopes.
  pub scopes: ScopesMap,
  /// Normalized integrity metadata.
  pub integrity: ModuleIntegrityMap,
}

/// A "module specifier map" with keys sorted in descending code-unit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSpecifierMap {
  /// Entries sorted in descending code-unit order by key.
  pub entries: Vec<(String, Option<Url>)>,
}

impl ModuleSpecifierMap {
  pub fn iter(&self) -> impl Iterator<Item = (&str, &Option<Url>)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v))
  }
}

/// A "scopes map" with scope prefixes sorted in descending code-unit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopesMap {
  /// Entries sorted in descending code-unit order by scope prefix.
  pub entries: Vec<(String, ModuleSpecifierMap)>,
}

impl ScopesMap {
  pub fn iter(&self) -> impl Iterator<Item = (&str, &ModuleSpecifierMap)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v))
  }
}

/// A normalized module integrity map.
///
/// Note: unlike `imports`/`scopes`, the HTML spec does not require sorting this map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleIntegrityMap {
  /// Entries in insertion order.
  pub entries: Vec<(String, String)>,
}

impl ModuleIntegrityMap {
  pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
  }
}

#[derive(Debug, Error)]
pub enum ImportMapError {
  /// JSON was syntactically invalid.
  #[error(transparent)]
  Json(#[from] serde_json::Error),
  /// The JSON value had the wrong type for the import map algorithm.
  #[error("TypeError: {0}")]
  TypeError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportMapWarning {
  pub kind: ImportMapWarningKind,
}

impl ImportMapWarning {
  pub fn new(kind: ImportMapWarningKind) -> Self {
    Self { kind }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportMapWarningKind {
  UnknownTopLevelKey { key: String },
  EmptySpecifierKey,
  AddressNotString { specifier_key: String },
  AddressInvalid { specifier_key: String, address: String },
  TrailingSlashMismatch { specifier_key: String, address: String },
  ScopePrefixNotParseable { prefix: String },
  IntegrityKeyFailedToResolve { key: String },
  IntegrityValueNotString { key: String },
}

pub(crate) fn code_unit_cmp(a: &str, b: &str) -> Ordering {
  let mut a_iter = a.encode_utf16();
  let mut b_iter = b.encode_utf16();
  loop {
    match (a_iter.next(), b_iter.next()) {
      (Some(a_unit), Some(b_unit)) => {
        let ord = a_unit.cmp(&b_unit);
        if ord != Ordering::Equal {
          return ord;
        }
      }
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (None, None) => return Ordering::Equal,
    }
  }
}

