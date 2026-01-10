use super::types::{
  code_unit_cmp, is_code_unit_prefix, ImportMap, ImportMapError, ImportMapParseResult, ImportMapState,
  ModuleIntegrityMap, ModuleSpecifierMap, SpecifierResolutionRecord,
};
use super::ImportMapLimits;

use std::collections::{HashMap, HashSet};

/// HTML: "merge module specifier maps".
///
/// Conflicts are resolved in favor of `old_map` (i.e. if a key exists in `old_map`, the entry from
/// `new_map` is ignored).
pub fn merge_module_specifier_maps(
  new_map: &ModuleSpecifierMap,
  old_map: &ModuleSpecifierMap,
) -> ModuleSpecifierMap {
  let mut merged = old_map.clone();

  for (specifier, url) in &new_map.entries {
    if old_map.contains_key(specifier) {
      continue;
    }
    merged.entries.push((specifier.clone(), url.clone()));
  }

  // Ensure resolution precedence remains correct: most-specific keys first.
  merged
    .entries
    .sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  merged
}

fn merge_integrity_maps(new_map: &ModuleIntegrityMap, old_map: &mut ModuleIntegrityMap) {
  for (url, integrity) in &new_map.entries {
    if old_map.contains_key(url) {
      continue;
    }
    old_map.entries.push((url.clone(), integrity.clone()));
  }
}

/// Index over the global object's "resolved module set" to accelerate import map merging.
///
/// HTML explicitly notes that the resolved module set can become large for real applications, and
/// encourages more efficient matching than naive nested iteration.
struct ResolvedModuleSetIndex<'a> {
  /// All resolved `record.specifier` values.
  ///
  /// Used for step 6 filtering: we need to know whether any resolved specifier is a prefix of a new
  /// import map key (string "starts with").
  resolved_specifiers: HashSet<&'a str>,

  /// Records grouped by scope prefix matches, keyed by:
  /// - the record's full `serialized_base_url` (for exact matches), and
  /// - every prefix of that base URL that ends with `/` (for the scope-prefix `ends with "/"` case).
  ///
  /// This makes scope filtering proportional to the number of records that actually match a scope.
  records_by_scope_prefix: HashMap<&'a str, Vec<&'a SpecifierResolutionRecord>>,
}

impl<'a> ResolvedModuleSetIndex<'a> {
  fn build(records: &'a [SpecifierResolutionRecord]) -> Self {
    let mut resolved_specifiers: HashSet<&'a str> = HashSet::with_capacity(records.len());
    let mut records_by_scope_prefix: HashMap<&'a str, Vec<&'a SpecifierResolutionRecord>> =
      HashMap::new();

    for record in records {
      resolved_specifiers.insert(record.specifier.as_str());

      let Some(base) = record.serialized_base_url.as_deref() else {
        continue;
      };

      // Exact base URL match (scopePrefix == record_base).
      records_by_scope_prefix
        .entry(base)
        .or_default()
        .push(record);

      // Prefix match buckets (scopePrefix ends with '/' AND record_base starts with scopePrefix).
      //
      // Only prefixes ending with '/' can ever match (per spec), so we pre-index just those.
      for (slash_idx, _) in base.match_indices('/') {
        let end = slash_idx + 1;
        if end == base.len() {
          // `base` was already indexed for the exact-match case above.
          continue;
        }
        records_by_scope_prefix
          .entry(&base[..end])
          .or_default()
          .push(record);
      }
    }

    Self {
      resolved_specifiers,
      records_by_scope_prefix,
    }
  }

  /// Spec step 6: whether a new top-level import key must be removed because it would impact an
  /// already-resolved specifier.
  fn new_import_key_is_blocked(&self, specifier: &str) -> bool {
    // Spec uses string "starts with": remove if there exists a resolved record whose `specifier` is
    // a prefix of the new key.
    //
    // Instead of scanning all resolved records, we only test prefixes of the candidate key and
    // check for exact membership in a set of resolved specifiers.
    //
    // Handle the empty-string prefix explicitly (char_indices yields no boundaries for empty
    // strings).
    if let Some(empty) = self.resolved_specifiers.get("") {
      if is_code_unit_prefix(empty, specifier) {
        return true;
      }
    }

    for (byte_idx, ch) in specifier.char_indices() {
      let end = byte_idx + ch.len_utf8();
      if let Some(matching) = self.resolved_specifiers.get(&specifier[..end]) {
        // Use code unit prefix semantics for extra correctness (matches Rust `starts_with` for
        // valid Unicode scalar strings).
        if is_code_unit_prefix(matching, specifier) {
          return true;
        }
      }
    }

    false
  }

  fn records_matching_scope_prefix(
    &self,
    scope_prefix: &str,
  ) -> Option<&Vec<&'a SpecifierResolutionRecord>> {
    self.records_by_scope_prefix.get(scope_prefix)
  }
}

/// HTML: "merge existing and new import maps".
///
/// This implements the resolved-module-set filtering rules that prevent new import maps from
/// retroactively changing resolution for already-resolved modules.
pub fn merge_existing_and_new_import_maps(
  state: &mut ImportMapState,
  new_import_map: &ImportMap,
) -> Result<(), ImportMapError> {
  merge_existing_and_new_import_maps_with_limits(state, new_import_map, &ImportMapLimits::default())
}

fn merge_existing_and_new_import_maps_impl(
  old_import_map: &mut ImportMap,
  resolved_module_set: &[SpecifierResolutionRecord],
  new_import_map: &ImportMap,
) {
  let resolved_index = ResolvedModuleSetIndex::build(resolved_module_set);

  // Step 1: deep copy of scopes that we will mutate when filtering out impacted rules.
  let new_scopes = new_import_map.scopes.clone();

  // Step 2: `oldImportMap` is `state.import_map` (mutated in place).

  // Step 3: deep copy of imports that we will mutate when filtering out impacted rules.
  let mut new_imports = new_import_map.imports.clone();

  // Step 4: merge scopes (after filtering by resolved module set).
  for (scope_prefix, mut scope_imports) in new_scopes.entries {
    if let Some(records) = resolved_index.records_matching_scope_prefix(scope_prefix.as_str()) {
      // If there are no new prefix keys, we can avoid building any of the prefix-match index.
      let has_prefix_keys = scope_imports.entries.iter().any(|(k, _)| k.ends_with('/'));

      // Exact-match removals.
      let mut exact_specifiers: HashSet<&str> = HashSet::with_capacity(records.len());

      // Prefix-match removals (only for records that permit prefix matches). We store a single
      // representative record specifier so we can perform the required code-unit-prefix check.
      let mut prefix_to_record_specifier: HashMap<&str, &str> = HashMap::new();

      for record in records {
        exact_specifiers.insert(record.specifier.as_str());

        if !has_prefix_keys || !record.as_url_kind.permits_prefix_match() {
          continue;
        }

        for (slash_idx, _) in record.specifier.match_indices('/') {
          let prefix = &record.specifier[..slash_idx + 1];
          prefix_to_record_specifier
            .entry(prefix)
            .or_insert(record.specifier.as_str());
        }
      }

      scope_imports.entries.retain(|(specifier_key, _)| {
        if exact_specifiers.contains(specifier_key.as_str()) {
          return false;
        }

        if specifier_key.ends_with('/') {
          if let Some(record_specifier) =
            prefix_to_record_specifier.get(specifier_key.as_str())
          {
            // Spec requires code-unit prefix semantics here.
            if is_code_unit_prefix(specifier_key.as_str(), record_specifier) {
              return false;
            }
          }
        }

        true
      });
    }

    if let Some((_, existing_scope_imports)) = old_import_map
      .scopes
      .entries
      .iter_mut()
      .find(|(prefix, _)| prefix == &scope_prefix)
    {
      *existing_scope_imports = merge_module_specifier_maps(&scope_imports, existing_scope_imports);
    } else {
      old_import_map.scopes.entries.push((scope_prefix, scope_imports));
    }
  }

  // Keep scopes sorted in descending code unit order.
  old_import_map
    .scopes
    .entries
    .sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  // Step 5: merge integrity (old wins on duplicates).
  merge_integrity_maps(&new_import_map.integrity, &mut old_import_map.integrity);

  // Step 6: filter new top-level imports that would impact already-resolved specifiers.
  new_imports.entries.retain(|(specifier, _)| {
    !resolved_index.new_import_key_is_blocked(specifier.as_str())
  });

  // Step 7: merge top-level imports (old wins on duplicates).
  old_import_map.imports = merge_module_specifier_maps(&new_imports, &old_import_map.imports);
}

/// Like [`merge_existing_and_new_import_maps`], but enforces deterministic [`ImportMapLimits`].
pub fn merge_existing_and_new_import_maps_with_limits(
  state: &mut ImportMapState,
  new_import_map: &ImportMap,
  limits: &ImportMapLimits,
) -> Result<(), ImportMapError> {
  // Validate inputs to keep behavior deterministic even if callers construct `ImportMap` directly.
  limits.validate_import_map(&state.import_map)?;
  limits.validate_import_map(new_import_map)?;

  // Merge into a clone so we can fail without partially mutating `state`.
  let mut merged = state.import_map.clone();
  merge_existing_and_new_import_maps_impl(&mut merged, &state.resolved_module_set, new_import_map);

  // Ensure the merged state remains within limits (prevents unbounded growth across many maps).
  limits.validate_import_map(&merged)?;
  state.import_map = merged;
  Ok(())
}

/// HTML: "register an import map".
///
/// If the parse result contained an error, this returns it and does not merge.
pub fn register_import_map(
  state: &mut ImportMapState,
  result: ImportMapParseResult,
) -> Result<(), ImportMapError> {
  register_import_map_with_limits(state, result, &ImportMapLimits::default())
}

/// Like [`register_import_map`], but enforces deterministic [`ImportMapLimits`] during merge.
pub fn register_import_map_with_limits(
  state: &mut ImportMapState,
  result: ImportMapParseResult,
  limits: &ImportMapLimits,
) -> Result<(), ImportMapError> {
  if let Some(err) = result.error_to_rethrow {
    return Err(err);
  }

  if let Some(import_map) = result.import_map {
    merge_existing_and_new_import_maps_with_limits(state, &import_map, limits)?;
  }

  Ok(())
}
