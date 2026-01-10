use url::Url;

use super::ImportMapState;

/// HTML: "resolve a module integrity metadata".
///
/// This looks up integrity metadata by the module script's serialized URL.
/// Returns the empty string when no integrity metadata is present.
pub fn resolve_module_integrity_metadata<'a>(state: &'a ImportMapState, url: &Url) -> &'a str {
  // The integrity map stores serialized URLs via `Url::to_string()`, which is equivalent to
  // `Url::as_str()` for lookup purposes.
  state.import_map.integrity.get(url.as_str()).unwrap_or("")
}

