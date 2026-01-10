use url::Url;

use super::ModuleSpecifierMap;

fn is_special_scheme(scheme: &str) -> bool {
  matches!(scheme, "ftp" | "file" | "http" | "https" | "ws" | "wss")
}

/// Resolve an imports match per WHATWG HTML ("resolve an imports match").
///
/// Return value:
/// - `None`: no match in the map
/// - `Some(None)`: matched a null entry (blocked / invalid mapping)
/// - `Some(Some(url))`: matched a URL entry, returning the resolved URL
pub fn resolve_imports_match(
  normalized_specifier: &str,
  as_url: Option<&Url>,
  specifier_map: &ModuleSpecifierMap,
) -> Option<Option<Url>> {
  let allow_prefix_matches = as_url.is_none() || as_url.is_some_and(|u| is_special_scheme(u.scheme()));

  for (specifier_key, resolution_result) in &specifier_map.entries {
    if specifier_key == normalized_specifier {
      return Some(resolution_result.clone());
    }

    if allow_prefix_matches
      && specifier_key.ends_with('/')
      && normalized_specifier.starts_with(specifier_key)
    {
      let Some(base) = resolution_result else {
        return Some(None);
      };

      // Spec invariant: enforced during parsing.
      if !base.as_str().ends_with('/') {
        debug_assert!(
          false,
          "import map invariant violation: prefix key \"{specifier_key}\" maps to non-slash URL \"{}\"",
          base.as_str()
        );
        return Some(None);
      }

      let after_prefix = &normalized_specifier[specifier_key.len()..];
      let Ok(url) = base.join(after_prefix) else {
        return Some(None);
      };

      // Backtracking protection: the resolved URL must not escape above the mapped prefix base.
      if !url.as_str().starts_with(base.as_str()) {
        return Some(None);
      }

      return Some(Some(url));
    }
  }

  None
}
