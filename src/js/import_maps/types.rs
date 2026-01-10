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

/// Record stored in the global object's "resolved module set".
///
/// This is used by the HTML Standard to avoid resolving the same specifier/base URL combination
/// multiple times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecifierResolutionRecord {
  pub serialized_base_url: Option<String>,
  /// The normalized specifier string (per the import maps spec).
  pub specifier: String,
  pub as_url_kind: SpecifierAsUrlKind,
}

/// The global object's "resolved module set".
pub type ResolvedModuleSet = Vec<SpecifierResolutionRecord>;

/// Host-side global state for import map resolution/merging.
#[derive(Debug, Clone, Default)]
pub struct ImportMapState {
  pub import_map: ImportMap,
  pub resolved_module_set: ResolvedModuleSet,
}

impl ImportMapState {
  pub fn new_empty() -> Self {
    Self::default()
  }

  pub fn import_map(&self) -> &ImportMap {
    &self.import_map
  }

  pub fn import_map_mut(&mut self) -> &mut ImportMap {
    &mut self.import_map
  }

  pub fn resolved_module_set(&self) -> &[SpecifierResolutionRecord] {
    &self.resolved_module_set
  }

  pub fn resolved_module_set_mut(&mut self) -> &mut ResolvedModuleSet {
    &mut self.resolved_module_set
  }
}

/// Whether a "specifier as a URL" is null, special, or non-special.
///
/// HTML notes that implementations can store this as a boolean (`asURL` is null OR special). We
/// keep the full tri-state so merge filtering remains readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecifierAsUrlKind {
  /// Specifier was not URL-like (`asURL` was null).
  NotUrl,
  /// Specifier was URL-like and the resulting URL is special (http/https/file/...).
  Special,
  /// Specifier was URL-like and the resulting URL is non-special (data:, blob:, ...).
  NonSpecial,
}

impl SpecifierAsUrlKind {
  /// Whether prefix matches are permitted when resolving/merging import map entries.
  pub fn permits_prefix_match(self) -> bool {
    matches!(self, Self::NotUrl | Self::Special)
  }

  /// Convenience helper matching the HTML Standard's `asURL is null or is special` checks.
  pub fn as_url_is_null_or_special(self) -> bool {
    self.permits_prefix_match()
  }
}

/// A "module specifier map" with keys sorted in descending code-unit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSpecifierMap {
  /// Entries sorted in descending code-unit order by key.
  pub entries: Vec<(String, Option<Url>)>,
}

impl ModuleSpecifierMap {
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &Option<Url>)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v))
  }

  /// Iterate entries in deterministic **descending** key order.
  ///
  /// This is equivalent to [`ModuleSpecifierMap::iter`].
  pub fn iter_descending(&self) -> impl Iterator<Item = (&str, &Option<Url>)> {
    self.iter()
  }

  pub fn contains_key(&self, key: &str) -> bool {
    self.get(key).is_some()
  }

  pub fn get(&self, key: &str) -> Option<&Option<Url>> {
    self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(key, k))
      .ok()
      .and_then(|idx| self.entries.get(idx).map(|(_, v)| v))
  }
}

/// A "scopes map" with scope prefixes sorted in descending code-unit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopesMap {
  /// Entries sorted in descending code-unit order by scope prefix.
  pub entries: Vec<(String, ModuleSpecifierMap)>,
}

impl ScopesMap {
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &ModuleSpecifierMap)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v))
  }

  /// Iterate scopes in deterministic **descending** key order.
  ///
  /// This is equivalent to [`ScopesMap::iter`].
  pub fn iter_descending(&self) -> impl Iterator<Item = (&str, &ModuleSpecifierMap)> {
    self.iter()
  }

  pub fn get(&self, scope_prefix: &str) -> Option<&ModuleSpecifierMap> {
    self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(scope_prefix, k))
      .ok()
      .and_then(|idx| self.entries.get(idx).map(|(_, v)| v))
  }
}

/// Alias for [`ScopesMap`].
///
/// The HTML Standard calls this a "scope map".
pub type ScopeMap = ScopesMap;

/// A normalized module integrity map.
///
/// Note: unlike `imports`/`scopes`, the HTML spec does not require sorting this map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleIntegrityMap {
  /// Entries in insertion order.
  pub entries: Vec<(String, String)>,
}

impl ModuleIntegrityMap {
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
    self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
  }

  pub fn contains_key(&self, key: &str) -> bool {
    self.entries.iter().any(|(k, _)| k == key)
  }

  pub fn get(&self, key: &str) -> Option<&str> {
    self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
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

/// Errors raised while resolving module specifiers using an import map.
///
/// This is currently an alias to [`ImportMapError`], which already contains the HTML spec's
/// TypeError conditions (null entries, backtracking, bare specifiers not mapped, ...).
pub type ModuleResolutionError = ImportMapError;

/// HTML "import map parse result" (script-element `result` slot value for `type="importmap"`).
#[derive(Debug, Default)]
pub struct ImportMapParseResult {
  /// The successfully parsed import map, or `None` if parsing failed.
  pub import_map: Option<ImportMap>,
  /// The error that prevented using this import map, if any.
  pub error_to_rethrow: Option<ImportMapError>,
  /// Non-fatal warnings encountered while parsing/normalizing.
  pub warnings: Vec<ImportMapWarning>,
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
