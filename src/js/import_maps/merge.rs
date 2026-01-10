use super::types::{
  code_unit_cmp, is_code_unit_prefix, ImportMap, ImportMapError, ImportMapParseResult, ImportMapState,
  ModuleIntegrityMap, ModuleSpecifierMap,
};

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

/// HTML: "merge existing and new import maps".
///
/// This implements the resolved-module-set filtering rules that prevent new import maps from
/// retroactively changing resolution for already-resolved modules.
pub fn merge_existing_and_new_import_maps(state: &mut ImportMapState, new_import_map: &ImportMap) {
  // Step 1: deep copy of scopes that we will mutate when filtering out impacted rules.
  let new_scopes = new_import_map.scopes.clone();

  // Step 2: `oldImportMap` is `state.import_map` (mutated in place).

  // Step 3: deep copy of imports that we will mutate when filtering out impacted rules.
  let mut new_imports = new_import_map.imports.clone();

  // Step 4: merge scopes (after filtering by resolved module set).
  for (scope_prefix, mut scope_imports) in new_scopes.entries {
    scope_imports.entries.retain(|(specifier_key, _)| {
      !state.resolved_module_set.iter().any(|record| {
        let Some(record_base) = record.serialized_base_url.as_deref() else {
          return false;
        };

        let base_matches = scope_prefix == record_base
          || (scope_prefix.ends_with('/')
            && is_code_unit_prefix(scope_prefix.as_str(), record_base));
        if !base_matches {
          return false;
        }

        specifier_key == &record.specifier
          || (specifier_key.ends_with('/')
            && is_code_unit_prefix(specifier_key.as_str(), record.specifier.as_str())
            && record.as_url_kind.permits_prefix_match())
      })
    });

    if let Some((_, existing_scope_imports)) = state
      .import_map
      .scopes
      .entries
      .iter_mut()
      .find(|(prefix, _)| prefix == &scope_prefix)
    {
      *existing_scope_imports = merge_module_specifier_maps(&scope_imports, existing_scope_imports);
    } else {
      state
        .import_map
        .scopes
        .entries
        .push((scope_prefix, scope_imports));
    }
  }

  // Keep scopes sorted in descending code unit order.
  state
    .import_map
    .scopes
    .entries
    .sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  // Step 5: merge integrity (old wins on duplicates).
  merge_integrity_maps(&new_import_map.integrity, &mut state.import_map.integrity);

  // Step 6: filter new top-level imports that would impact already-resolved specifiers.
  new_imports.entries.retain(|(specifier, _)| {
    !state
      .resolved_module_set
      .iter()
      .any(|record| is_code_unit_prefix(record.specifier.as_str(), specifier.as_str()))
  });

  // Step 7: merge top-level imports (old wins on duplicates).
  state.import_map.imports = merge_module_specifier_maps(&new_imports, &state.import_map.imports);
}

/// HTML: "register an import map".
///
/// If the parse result contained an error, this returns it and does not merge.
pub fn register_import_map(
  state: &mut ImportMapState,
  result: ImportMapParseResult,
) -> Result<(), ImportMapError> {
  if let Some(err) = result.error_to_rethrow {
    return Err(err);
  }

  if let Some(import_map) = result.import_map {
    merge_existing_and_new_import_maps(state, &import_map);
  }

  Ok(())
}
