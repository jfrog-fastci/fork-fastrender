use super::types::{ImportMap, ImportMapError};

/// Deterministic resource limits for import map parsing and merging.
///
/// Import maps are attacker-controlled JSON. Even relatively small inputs can encode tens of
/// thousands of entries (many tiny strings), causing large allocations and O(N) work.
///
/// These limits are enforced:
/// - during parsing (`parse_import_map_string_with_limits` / `create_import_map_parse_result_with_limits`)
/// - during registration/merge (`register_import_map_with_limits`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportMapLimits {
  /// Maximum number of bytes accepted by import map parsing *before* attempting to parse JSON.
  ///
  /// This is intentionally separate from `JsExecutionOptions::max_script_bytes` so non-HTML callers
  /// (or hosts that parse import maps out-of-band) are still protected.
  pub max_bytes: usize,

  /// Maximum number of entries in the top-level `"imports"` object.
  pub max_imports_entries: usize,

  /// Maximum number of scope prefixes in the top-level `"scopes"` object.
  pub max_scopes: usize,

  /// Maximum number of entries in a single scope's module specifier map.
  pub max_scope_entries: usize,

  /// Maximum number of entries in the top-level `"integrity"` object.
  pub max_integrity_entries: usize,

  /// Maximum total number of entries across the import map.
  ///
  /// This is applied to the merged state too to prevent an attacker from registering many small
  /// import maps and growing global state without bound.
  ///
  /// Total entries are counted as:
  /// - `imports.entries.len()`
  /// - `integrity.entries.len()`
  /// - `scopes.entries.len()` (scope prefixes)
  /// - the sum of `scope.entries.len()` for all scopes
  pub max_total_entries: usize,

  /// Maximum number of bytes in any specifier key / scope prefix / integrity key string.
  pub max_key_bytes: usize,

  /// Maximum number of bytes in any address string / integrity value string.
  pub max_value_bytes: usize,
}

impl Default for ImportMapLimits {
  fn default() -> Self {
    Self {
      // Large enough for realistic import maps, small enough to bound hostile JSON.
      max_bytes: 2 * 1024 * 1024, // 2 MiB

      // Allow large single-table maps, but keep the global cap small-ish.
      max_imports_entries: 10_000,
      max_scopes: 1024,
      max_scope_entries: 1024,
      max_integrity_entries: 10_000,

      // Conservative "total rules" cap (imports + integrity + scopes + scoped rules).
      max_total_entries: 10_000,

      // Keys/values are attacker-controlled strings; cap them to avoid pathological URL parsing and
      // large per-entry allocations.
      max_key_bytes: 2048,
      max_value_bytes: 4096,
    }
  }
}

impl ImportMapLimits {
  pub fn validate_import_map(&self, map: &ImportMap) -> Result<(), ImportMapError> {
    if map.imports.len() > self.max_imports_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"imports\" has too many entries ({} > max {})",
        map.imports.len(),
        self.max_imports_entries
      )));
    }

    if map.scopes.len() > self.max_scopes {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"scopes\" has too many scope prefixes ({} > max {})",
        map.scopes.len(),
        self.max_scopes
      )));
    }

    for (scope_prefix, scope_map) in map.scopes.iter() {
      if scope_map.len() > self.max_scope_entries {
        return Err(ImportMapError::LimitExceeded(format!(
          "scope {scope_prefix:?} has too many entries ({} > max {})",
          scope_map.len(),
          self.max_scope_entries
        )));
      }
    }

    if map.integrity.len() > self.max_integrity_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"integrity\" has too many entries ({} > max {})",
        map.integrity.len(),
        self.max_integrity_entries
      )));
    }

    let mut total_entries = 0usize;
    total_entries = total_entries.saturating_add(map.imports.len());
    total_entries = total_entries.saturating_add(map.integrity.len());
    total_entries = total_entries.saturating_add(map.scopes.len());
    for (_, scope_map) in map.scopes.iter() {
      total_entries = total_entries.saturating_add(scope_map.len());
    }

    if total_entries > self.max_total_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "import map has too many total entries ({total_entries} > max {})",
        self.max_total_entries
      )));
    }

    // Key/value byte lengths (post-normalization) to keep the stored state bounded.
    for (k, v) in map.imports.iter() {
      if k.len() > self.max_key_bytes {
        return Err(ImportMapError::LimitExceeded(format!(
          "\"imports\" key exceeded max_key_bytes ({} > max {})",
          k.len(),
          self.max_key_bytes
        )));
      }
      if let Some(url) = v {
        if url.as_str().len() > self.max_value_bytes {
          return Err(ImportMapError::LimitExceeded(format!(
            "\"imports\" address for key {k:?} exceeded max_value_bytes ({} > max {})",
            url.as_str().len(),
            self.max_value_bytes
          )));
        }
      }
    }

    for (scope_prefix, scope_map) in map.scopes.iter() {
      if scope_prefix.len() > self.max_key_bytes {
        return Err(ImportMapError::LimitExceeded(format!(
          "\"scopes\" prefix exceeded max_key_bytes ({} > max {})",
          scope_prefix.len(),
          self.max_key_bytes
        )));
      }
      for (k, v) in scope_map.iter() {
        if k.len() > self.max_key_bytes {
          return Err(ImportMapError::LimitExceeded(format!(
            "\"scopes\" key exceeded max_key_bytes ({} > max {})",
            k.len(),
            self.max_key_bytes
          )));
        }
        if let Some(url) = v {
          if url.as_str().len() > self.max_value_bytes {
            return Err(ImportMapError::LimitExceeded(format!(
              "\"scopes\" address for key {k:?} exceeded max_value_bytes ({} > max {})",
              url.as_str().len(),
              self.max_value_bytes
            )));
          }
        }
      }
    }

    for (k, v) in map.integrity.iter() {
      if k.len() > self.max_key_bytes {
        return Err(ImportMapError::LimitExceeded(format!(
          "\"integrity\" key exceeded max_key_bytes ({} > max {})",
          k.len(),
          self.max_key_bytes
        )));
      }
      if v.len() > self.max_value_bytes {
        return Err(ImportMapError::LimitExceeded(format!(
          "\"integrity\" value for key {k:?} exceeded max_value_bytes ({} > max {})",
          v.len(),
          self.max_value_bytes
        )));
      }
    }

    Ok(())
  }
}

