use url::Url;

use super::parse::resolve_url_like_module_specifier;
use super::types::{
  code_unit_prefix_candidates_ending_with_slash, is_code_unit_prefix, ImportMapError, ImportMapState,
  ModuleSpecifierMap, SpecifierAsUrlKind, SpecifierResolutionRecord,
};

fn is_special_url(url: &Url) -> bool {
  matches!(
    url.scheme(),
    "ftp" | "file" | "http" | "https" | "ws" | "wss"
  )
}

fn resolve_imports_match_impl(
  normalized_specifier: &str,
  as_url: Option<&Url>,
  specifier_map: &ModuleSpecifierMap,
) -> Result<Option<Url>, ImportMapError> {
  let allow_prefix_matches = as_url.map(is_special_url).unwrap_or(true);

  // Fast path: exact match (binary search).
  if let Some(resolution_result) = specifier_map.get(normalized_specifier) {
    let Some(url) = resolution_result else {
      return Err(ImportMapError::TypeError(format!(
        "resolution of {normalized_specifier} was blocked by a null entry."
      )));
    };
    return Ok(Some(url.clone()));
  }

  if !allow_prefix_matches {
    return Ok(None);
  }

  // Prefix matches: try candidate prefixes ending with "/" from most specific to least specific.
  for specifier_key in code_unit_prefix_candidates_ending_with_slash(normalized_specifier) {
    let Some(resolution_result) = specifier_map.get(specifier_key) else {
      continue;
    };

    let Some(base_url) = resolution_result else {
      return Err(ImportMapError::TypeError(format!(
        "resolution of {specifier_key} was blocked by a null entry."
      )));
    };
    debug_assert!(
      base_url.as_str().ends_with('/'),
      "parser must enforce trailing-slash invariant"
    );

    let after_prefix = &normalized_specifier[specifier_key.len()..];
    let url = base_url.join(after_prefix).map_err(|_| {
      ImportMapError::TypeError(format!(
        "resolution of {normalized_specifier} was blocked since the afterPrefix portion could not be URL-parsed relative to the resolutionResult mapped to by the {specifier_key} prefix."
      ))
    })?;

    if !is_code_unit_prefix(base_url.as_str(), url.as_str()) {
      return Err(ImportMapError::TypeError(format!(
        "resolution of {normalized_specifier} was blocked due to it backtracking above its prefix {specifier_key}."
      )));
    }

    return Ok(Some(url));
  }

  Ok(None)
}

/// WHATWG HTML: "resolve an imports match".
pub fn resolve_imports_match(
  normalized_specifier: &str,
  as_url: Option<&Url>,
  specifier_map: &ModuleSpecifierMap,
) -> Result<Option<Url>, ImportMapError> {
  resolve_imports_match_impl(normalized_specifier, as_url, specifier_map)
}

/// WHATWG HTML: "add module to resolved module set".
pub fn add_module_to_resolved_module_set(
  state: &mut ImportMapState,
  serialized_base_url: String,
  normalized_specifier: String,
  as_url: Option<&Url>,
) {
  let as_url_kind = match as_url {
    None => SpecifierAsUrlKind::NotUrl,
    Some(url) if is_special_url(url) => SpecifierAsUrlKind::Special,
    Some(_) => SpecifierAsUrlKind::NonSpecial,
  };

  state.resolved_module_set.push_record(SpecifierResolutionRecord {
    serialized_base_url: Some(serialized_base_url),
    specifier: normalized_specifier,
    as_url_kind,
  });
}

/// WHATWG HTML: "resolve a module specifier" (host-facing entry point).
pub fn resolve_module_specifier(
  state: &mut ImportMapState,
  specifier: &str,
  base_url: &Url,
) -> Result<Url, ImportMapError> {
  let serialized_base_url = base_url.to_string();

  let as_url = resolve_url_like_module_specifier(specifier, base_url);
  let normalized_specifier = as_url
    .as_ref()
    .map(|url| url.to_string())
    .unwrap_or_else(|| specifier.to_string());

  let mut result: Option<Url> = None;

  if let Some(scope_imports) = state.import_map.scopes.get(&serialized_base_url) {
    let match_result =
      resolve_imports_match_impl(normalized_specifier.as_str(), as_url.as_ref(), scope_imports)?;
    if match_result.is_some() {
      result = match_result;
    }
  }

  if result.is_none() {
    for scope_prefix in code_unit_prefix_candidates_ending_with_slash(&serialized_base_url) {
      if scope_prefix.len() == serialized_base_url.len() {
        continue;
      }
      let Some(scope_imports) = state.import_map.scopes.get(scope_prefix) else {
        continue;
      };

      let match_result = resolve_imports_match_impl(
        normalized_specifier.as_str(),
        as_url.as_ref(),
        scope_imports,
      )?;
      if match_result.is_some() {
        result = match_result;
        break;
      }
    }
  }

  if result.is_none() {
    result = resolve_imports_match_impl(
      normalized_specifier.as_str(),
      as_url.as_ref(),
      &state.import_map.imports,
    )?;
  }

  if result.is_none() {
    result = as_url.clone();
  }

  if let Some(url) = result {
    add_module_to_resolved_module_set(
      state,
      serialized_base_url,
      normalized_specifier,
      as_url.as_ref(),
    );
    return Ok(url);
  }

  Err(ImportMapError::TypeError(format!(
    "{specifier} was a bare specifier, but was not remapped to anything by the import map."
  )))
}
