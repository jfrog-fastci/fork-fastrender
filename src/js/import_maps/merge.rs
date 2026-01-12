use rustc_hash::FxHashSet;

use super::types::{
  code_unit_cmp, ImportMap, ImportMapError, ImportMapParseResult, ImportMapState,
  ModuleIntegrityMap, ModuleSpecifierMap, ResolvedModuleSetIndex,
};
use super::ImportMapLimits;

pub(crate) trait MergeInstrumentation {
  fn on_scope_record_scanned(&mut self) {}
  fn on_scope_key_checked(&mut self) {}
  fn on_scope_prefix_query(&mut self) {}
  fn on_top_level_import_key_checked(&mut self) {}
}

impl MergeInstrumentation for () {}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MergeStats {
  pub scope_records_scanned: usize,
  pub scope_keys_checked: usize,
  pub scope_prefix_queries: usize,
  pub top_level_import_keys_checked: usize,
}

#[cfg(test)]
impl MergeInstrumentation for MergeStats {
  fn on_scope_record_scanned(&mut self) {
    self.scope_records_scanned += 1;
  }

  fn on_scope_key_checked(&mut self) {
    self.scope_keys_checked += 1;
  }

  fn on_scope_prefix_query(&mut self) {
    self.scope_prefix_queries += 1;
  }

  fn on_top_level_import_key_checked(&mut self) {
    self.top_level_import_keys_checked += 1;
  }
}

fn any_with_prefix<I: MergeInstrumentation>(sorted: &[&str], prefix: &str, instr: &mut I) -> bool {
  instr.on_scope_prefix_query();
  let start = sorted.partition_point(|s| *s < prefix);
  sorted
    .get(start)
    .map_or(false, |candidate| candidate.starts_with(prefix))
}

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
pub fn merge_existing_and_new_import_maps(
  state: &mut ImportMapState,
  new_import_map: &ImportMap,
) -> Result<(), ImportMapError> {
  merge_existing_and_new_import_maps_with_limits(state, new_import_map, &ImportMapLimits::default())
}

fn merge_existing_and_new_import_maps_impl(
  old_import_map: &mut ImportMap,
  resolved_module_set: &ResolvedModuleSetIndex,
  new_import_map: &ImportMap,
) {
  let mut instr = ();
  merge_existing_and_new_import_maps_impl_instrumented(
    old_import_map,
    resolved_module_set,
    new_import_map,
    &mut instr,
  );
}

pub(crate) fn merge_existing_and_new_import_maps_impl_instrumented<I: MergeInstrumentation>(
  old_import_map: &mut ImportMap,
  resolved_module_set: &ResolvedModuleSetIndex,
  new_import_map: &ImportMap,
  instr: &mut I,
) {
  // Step 1: deep copy of scopes that we will mutate when filtering out impacted rules.
  let new_scopes = new_import_map.scopes.clone();

  // Step 3: deep copy of imports that we will mutate when filtering out impacted rules.
  let mut new_imports = new_import_map.imports.clone();

  // Step 4: merge scopes (after filtering by resolved module set).
  for (scope_prefix, mut scope_imports) in new_scopes.entries {
    let mut exact_specifiers: FxHashSet<&str> = FxHashSet::default();
    let mut prefix_allowed_specifiers: Vec<&str> = Vec::new();

    for record in resolved_module_set.iter_records_matching_scope_prefix(scope_prefix.as_str()) {
      instr.on_scope_record_scanned();
      exact_specifiers.insert(record.specifier.as_str());
      if record.as_url_kind.permits_prefix_match() {
        prefix_allowed_specifiers.push(record.specifier.as_str());
      }
    }

    if !exact_specifiers.is_empty() || !prefix_allowed_specifiers.is_empty() {
      prefix_allowed_specifiers.sort_unstable();
      scope_imports.entries.retain(|(specifier_key, _)| {
        instr.on_scope_key_checked();
        if exact_specifiers.contains(specifier_key.as_str()) {
          return false;
        }
        if specifier_key.ends_with('/')
          && any_with_prefix(&prefix_allowed_specifiers, specifier_key.as_str(), instr)
        {
          return false;
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
      old_import_map
        .scopes
        .entries
        .push((scope_prefix, scope_imports));
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
    instr.on_top_level_import_key_checked();
    !resolved_module_set.new_import_key_impacts_resolved_module(specifier.as_str())
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

  // `resolved_module_set` can grow large during module loading; keep its base-url index sorted only
  // when we need to perform scope-prefix queries (during merge).
  state.resolved_module_set.ensure_base_url_index_sorted();

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
