//! JavaScript host integration utilities.
//!
//! This module is intentionally small and focused: it provides DOM-to-scheduler bridging helpers
//! such as `<script>` element extraction. Full JS execution + event loop integration will be built
//! incrementally on top of these primitives.

pub mod dom_scripts;

pub use dom_scripts::extract_script_elements;

/// The script processing mode for a `<script>` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptType {
  /// A classic script (default when `type` is missing/empty or a JS MIME type).
  Classic,
  /// An ECMAScript module script (`type="module"`).
  Module,
  /// An import map (`type="importmap"`).
  ImportMap,
  /// An unrecognized script type (not executable by the HTML script processing model).
  Unknown,
}

/// A parsed `<script>` element, normalized into a scheduler-friendly record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptElementSpec {
  /// The document base URL used to resolve relative script URLs, if known.
  pub base_url: Option<String>,
  /// The resolved `src` URL, if present and resolvable.
  pub src: Option<String>,
  /// The concatenated inline script text from child text nodes.
  pub inline_text: String,
  /// Whether the `async` boolean attribute is present.
  pub async_attr: bool,
  /// Whether the `defer` boolean attribute is present.
  pub defer_attr: bool,
  /// Whether the script was inserted by the HTML parser.
  ///
  /// For now, DOM-extracted scripts are always treated as parser-inserted; later integration with
  /// the HTML parser can set this more precisely.
  pub parser_inserted: bool,
  /// The script type (classic/module/importmap/unknown) derived from element attributes.
  pub script_type: ScriptType,
}

/// Determine the script type for a `<script>` element based on `type`/`language` attributes.
///
/// This is a conservative subset of the HTML Standard behavior:
/// - `type` missing/empty => classic script
/// - `type="module"` => module script
/// - `type="importmap"` => import map
/// - Known JavaScript MIME types => classic script
/// - Otherwise => unknown (non-executable) script
pub fn determine_script_type(script: &crate::dom::DomNode) -> ScriptType {
  let Some(tag_name) = script.tag_name() else {
    return ScriptType::Unknown;
  };
  if !tag_name.eq_ignore_ascii_case("script") {
    return ScriptType::Unknown;
  }

  let mut type_value = script
    .get_attribute_ref("type")
    .map(str::trim)
    .filter(|value| !value.is_empty());

  // Treat `type=""` as missing.
  if type_value.is_none() {
    // The obsolete `language` attribute can still appear on real pages; treat it as a hint only
    // when no `type` is present.
    if let Some(language) = script
      .get_attribute_ref("language")
      .map(str::trim)
      .filter(|value| !value.is_empty())
    {
      // Legacy values are typically things like `javascript` / `javascript1.5`.
      if language.to_ascii_lowercase().starts_with("javascript") {
        return ScriptType::Classic;
      }
      return ScriptType::Unknown;
    }

    return ScriptType::Classic;
  }

  let type_value_str = type_value.take().unwrap_or_default();
  let mime_essence = type_value_str
    .split_once(';')
    .map(|(essence, _)| essence.trim())
    .unwrap_or(type_value_str);

  if mime_essence.eq_ignore_ascii_case("module") {
    return ScriptType::Module;
  }
  if mime_essence.eq_ignore_ascii_case("importmap") {
    return ScriptType::ImportMap;
  }

  // Recognize common JavaScript MIME types.
  const CLASSIC_JS_MIME_TYPES: [&str; 4] = [
    "text/javascript",
    "application/javascript",
    "text/ecmascript",
    "application/ecmascript",
  ];
  if CLASSIC_JS_MIME_TYPES
    .iter()
    .any(|ty| mime_essence.eq_ignore_ascii_case(ty))
  {
    return ScriptType::Classic;
  }

  // Legacy JavaScript types seen in the wild.
  const LEGACY_JS_TYPES: [&str; 8] = [
    "application/x-javascript",
    "application/x-ecmascript",
    "text/x-javascript",
    "text/x-ecmascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
  ];
  if LEGACY_JS_TYPES
    .iter()
    .any(|ty| mime_essence.eq_ignore_ascii_case(ty))
  {
    return ScriptType::Classic;
  }

  ScriptType::Unknown
}

