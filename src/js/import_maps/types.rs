use std::cmp::Ordering;
use std::ops::Deref;

use rustc_hash::FxHashMap;
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

#[derive(Debug, Clone)]
struct SpecifierPrefixTrie {
  terminals: Vec<bool>,
  edges: FxHashMap<u64, usize>,
}

impl Default for SpecifierPrefixTrie {
  fn default() -> Self {
    Self {
      terminals: vec![false],
      edges: FxHashMap::default(),
    }
  }
}

impl SpecifierPrefixTrie {
  #[inline]
  fn edge_key(node: usize, byte: u8) -> u64 {
    ((node as u64) << 8) | (byte as u64)
  }

  fn insert(&mut self, s: &str) {
    let mut node = 0usize;
    for &b in s.as_bytes() {
      let key = Self::edge_key(node, b);
      let next = if let Some(&next) = self.edges.get(&key) {
        next
      } else {
        let next = self.terminals.len();
        self.terminals.push(false);
        self.edges.insert(key, next);
        next
      };
      node = next;
    }
    self.terminals[node] = true;
  }

  fn has_prefix_of(&self, s: &str) -> bool {
    let mut node = 0usize;
    for &b in s.as_bytes() {
      let key = Self::edge_key(node, b);
      let Some(&next) = self.edges.get(&key) else {
        return false;
      };
      node = next;
      if self.terminals[node] {
        return true;
      }
    }
    false
  }
}

/// An index over the global object's "resolved module set" records.
///
/// The HTML Standard's import map merging algorithm needs to efficiently determine which new map
/// entries would impact already-resolved modules. The spec explicitly notes that implementations
/// should avoid naive nested scans over the resolved module set.
#[derive(Debug, Clone)]
pub struct ResolvedModuleSetIndex {
  records: Vec<SpecifierResolutionRecord>,
  /// Record indices for entries that have a `serialized_base_url`.
  ///
  /// This is kept sorted lexicographically by base URL **on demand**.
  ///
  /// This supports quickly finding records matching a scope prefix:
  /// - exact match, OR
  /// - if the scope prefix ends with `/`, prefix match.
  base_url_index: Vec<usize>,
  base_url_index_sorted: bool,
  /// Fast "does any resolved specifier prefix this key?" check for top-level import filtering.
  specifier_prefix_trie: SpecifierPrefixTrie,
}

impl Default for ResolvedModuleSetIndex {
  fn default() -> Self {
    Self {
      records: Vec::new(),
      base_url_index: Vec::new(),
      base_url_index_sorted: true,
      specifier_prefix_trie: SpecifierPrefixTrie::default(),
    }
  }
}

impl Deref for ResolvedModuleSetIndex {
  type Target = [SpecifierResolutionRecord];

  fn deref(&self) -> &Self::Target {
    &self.records
  }
}

impl ResolvedModuleSetIndex {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn from_records(records: Vec<SpecifierResolutionRecord>) -> Self {
    let mut base_url_index: Vec<usize> = Vec::new();
    let mut specifier_prefix_trie = SpecifierPrefixTrie::default();

    for (idx, record) in records.iter().enumerate() {
      if record.serialized_base_url.is_some() {
        base_url_index.push(idx);
      }
      specifier_prefix_trie.insert(record.specifier.as_str());
    }

    {
      let records = records.as_slice();
      base_url_index.sort_unstable_by(|a, b| {
        records[*a]
          .serialized_base_url
          .as_deref()
          .unwrap()
          .cmp(records[*b].serialized_base_url.as_deref().unwrap())
      });
    }

    Self {
      records,
      base_url_index,
      base_url_index_sorted: true,
      specifier_prefix_trie,
    }
  }

  pub fn push_record(&mut self, record: SpecifierResolutionRecord) {
    let idx = self.records.len();
    if record.serialized_base_url.is_some() {
      self.base_url_index.push(idx);
      self.base_url_index_sorted = false;
    }
    self.specifier_prefix_trie.insert(record.specifier.as_str());
    self.records.push(record);
  }

  pub(crate) fn ensure_base_url_index_sorted(&mut self) {
    if self.base_url_index_sorted {
      return;
    }
    let records = self.records.as_slice();
    self.base_url_index.sort_unstable_by(|a, b| {
      records[*a]
        .serialized_base_url
        .as_deref()
        .unwrap()
        .cmp(records[*b].serialized_base_url.as_deref().unwrap())
    });
    self.base_url_index_sorted = true;
  }

  pub(crate) fn iter_records_matching_scope_prefix<'a>(
    &'a self,
    scope_prefix: &'a str,
  ) -> impl Iterator<Item = &'a SpecifierResolutionRecord> + 'a {
    debug_assert!(
      self.base_url_index_sorted,
      "base_url_index must be sorted before calling iter_records_matching_scope_prefix; call ensure_base_url_index_sorted()"
    );
    let scope_prefix_ends_with_slash = scope_prefix.ends_with('/');
    let start = self.base_url_index.partition_point(|idx| {
      self.records[*idx].serialized_base_url.as_deref().unwrap() < scope_prefix
    });
    self.base_url_index[start..]
      .iter()
      .take_while(move |idx| {
        let base_url = self.records[**idx].serialized_base_url.as_deref().unwrap();
        if scope_prefix_ends_with_slash {
          base_url.starts_with(scope_prefix)
        } else {
          base_url == scope_prefix
        }
      })
      .map(move |idx| &self.records[*idx])
  }

  pub(crate) fn new_import_key_impacts_resolved_module(&self, specifier: &str) -> bool {
    self.specifier_prefix_trie.has_prefix_of(specifier)
  }
}

/// The global object's "resolved module set".
pub type ResolvedModuleSet = ResolvedModuleSetIndex;

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

  /// HTML: "resolve a module integrity metadata".
  ///
  /// Convenience wrapper around [`crate::js::import_maps::resolve_module_integrity_metadata`].
  pub fn resolve_module_integrity_metadata(&self, url: &Url) -> &str {
    super::integrity::resolve_module_integrity_metadata(self, url)
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

  /// Insert or replace an entry, maintaining deterministic descending key order.
  ///
  /// Returns the previous value for `key`, if any.
  pub fn insert(&mut self, key: String, value: Option<Url>) -> Option<Option<Url>> {
    match self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(&key, k))
    {
      Ok(idx) => Some(std::mem::replace(&mut self.entries[idx].1, value)),
      Err(idx) => {
        self.entries.insert(idx, (key, value));
        None
      }
    }
  }

  /// Remove an entry, if it exists, preserving sort order.
  pub fn remove(&mut self, key: &str) -> Option<Option<Url>> {
    match self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(key, k))
    {
      Ok(idx) => Some(self.entries.remove(idx).1),
      Err(_) => None,
    }
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

  /// Insert or replace a scope entry, maintaining deterministic descending key order.
  ///
  /// Returns the previous module specifier map for `scope_prefix`, if any.
  pub fn insert(
    &mut self,
    scope_prefix: String,
    scope_map: ModuleSpecifierMap,
  ) -> Option<ModuleSpecifierMap> {
    match self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(&scope_prefix, k))
    {
      Ok(idx) => Some(std::mem::replace(&mut self.entries[idx].1, scope_map)),
      Err(idx) => {
        self.entries.insert(idx, (scope_prefix, scope_map));
        None
      }
    }
  }

  /// Remove a scope entry, if it exists, preserving sort order.
  pub fn remove(&mut self, scope_prefix: &str) -> Option<ModuleSpecifierMap> {
    match self
      .entries
      .binary_search_by(|(k, _)| code_unit_cmp(scope_prefix, k))
    {
      Ok(idx) => Some(self.entries.remove(idx).1),
      Err(_) => None,
    }
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
    self
      .entries
      .iter()
      .find(|(k, _)| k == key)
      .map(|(_, v)| v.as_str())
  }

  /// Insert or replace an integrity entry.
  ///
  /// Unlike `imports`/`scopes`, the HTML Standard does not require sorting this map; entries are
  /// kept in insertion order (updates keep the existing entry position).
  pub fn insert(&mut self, key: String, integrity: String) -> Option<String> {
    if let Some((_, existing)) = self.entries.iter_mut().find(|(k, _)| k == &key) {
      return Some(std::mem::replace(existing, integrity));
    }
    self.entries.push((key, integrity));
    None
  }

  /// Remove an integrity entry, if it exists.
  pub fn remove(&mut self, key: &str) -> Option<String> {
    let idx = self.entries.iter().position(|(k, _)| k == key)?;
    Some(self.entries.remove(idx).1)
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
  /// A deterministic resource limit was exceeded while parsing or merging an import map.
  #[error("TypeError: import map limit exceeded: {0}")]
  LimitExceeded(String),
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
  UnknownTopLevelKey {
    key: String,
  },
  EmptySpecifierKey,
  AddressNotString {
    specifier_key: String,
  },
  AddressInvalid {
    specifier_key: String,
    address: String,
  },
  TrailingSlashMismatch {
    specifier_key: String,
    address: String,
  },
  ScopePrefixNotParseable {
    prefix: String,
  },
  IntegrityKeyFailedToResolve {
    key: String,
  },
  IntegrityValueNotString {
    key: String,
  },
}

pub(crate) fn code_unit_cmp(a: &str, b: &str) -> Ordering {
  super::strings::cmp_code_units(a, b)
}

/// Whether `prefix` is a prefix of `full` when both strings are compared as sequences of UTF-16
/// code units.
///
/// This matches the HTML Standard's "code unit prefix" definition (as used by import maps).
///
/// Note: Rust `&str` values are always valid UTF-8 and therefore cannot represent unpaired
/// surrogates. The HTML Standard defines this operation over arbitrary sequences of UTF-16 code
/// units (including lone surrogates), so this helper intentionally implements the semantics on the
/// UTF-16 *encoding* of each string.
pub(super) fn is_code_unit_prefix(prefix: &str, full: &str) -> bool {
  let mut prefix_iter = prefix.encode_utf16();
  let mut full_iter = full.encode_utf16();
  loop {
    match prefix_iter.next() {
      None => return true,
      Some(prefix_unit) => match full_iter.next() {
        Some(full_unit) if full_unit == prefix_unit => continue,
        _ => return false,
      },
    }
  }
}

/// Enumerate all prefixes of `s` that end with `/`, in descending (most-specific-first) order,
/// using UTF-16 code unit indexing semantics.
///
/// This is used to implement the import maps prefix matching rules without relying on UTF-8 byte
/// indices. Returned slices are guaranteed to be valid UTF-8 (`&str` slices).
///
/// If a computed code unit boundary does not correspond to a UTF-8 character boundary (e.g. would
/// split a surrogate pair), it is skipped. This can only occur if the underlying string contains
/// unpaired surrogates, which cannot be represented in Rust `&str` values.
pub(super) fn code_unit_prefix_candidates_ending_with_slash<'a>(s: &'a str) -> Vec<&'a str> {
  // Step 1: collect the code-unit offsets (end positions) of every `/` code unit.
  let slash = '/' as u16;
  let mut slash_ends_in_code_units: Vec<usize> = Vec::new();
  for (idx, unit) in s.encode_utf16().enumerate() {
    if unit == slash {
      slash_ends_in_code_units.push(idx + 1);
    }
  }

  if slash_ends_in_code_units.is_empty() {
    return Vec::new();
  }

  // Step 2: Walk the UTF-8 string once, tracking the current UTF-16 code unit offset, and map each
  // desired code-unit end offset to a UTF-8 byte index that Rust can slice on.
  let mut out: Vec<&'a str> = Vec::with_capacity(slash_ends_in_code_units.len());
  let mut next_slash_idx = 0usize;
  let mut code_units_so_far = 0usize;
  for (byte_idx, ch) in s.char_indices() {
    code_units_so_far += ch.len_utf16();
    while next_slash_idx < slash_ends_in_code_units.len()
      && code_units_so_far == slash_ends_in_code_units[next_slash_idx]
    {
      debug_assert_eq!(
        ch, '/',
        "code unit offset for '/' must coincide with end of a '/' character"
      );
      out.push(&s[..byte_idx + ch.len_utf8()]);
      next_slash_idx += 1;
    }
  }

  // Most-specific-first.
  out.reverse();
  out
}
