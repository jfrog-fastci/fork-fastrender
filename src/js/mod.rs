//! JavaScript host integration utilities.
//!
//! This module is intentionally small and focused: it provides DOM-to-scheduler bridging helpers
//! such as `<script>` element extraction. Full JS execution + event loop integration will be built
//! incrementally on top of these primitives.

pub mod dom_scripts;
pub mod clock;
pub mod event_loop;

pub use dom_scripts::extract_script_elements;
pub use clock::{Clock, RealClock, VirtualClock};
pub use event_loop::{
  EventLoop, QueueLimits, RunLimits, RunUntilIdleOutcome, RunUntilIdleStopReason, Task, TaskSource,
  TimerId,
};

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

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Notably, this does *not* include U+000B VT (vertical tab).
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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
/// This follows the HTML Standard script preparation rules for computing the script block type
/// string and then mapping it to `classic`/`module`/`importmap`/unknown.
pub fn determine_script_type(script: &crate::dom::DomNode) -> ScriptType {
  let Some(tag_name) = script.tag_name() else {
    return ScriptType::Unknown;
  };
  if !tag_name.eq_ignore_ascii_case("script") {
    return ScriptType::Unknown;
  }

  // Compute the "script block's type string" per the HTML Standard:
  // - `type=""` => defaults to `text/javascript`
  // - no `type` + `language=""` => defaults to `text/javascript`
  // - no `type` + no `language` => defaults to `text/javascript`
  // - otherwise:
  //   - `type=<value>` => ASCII whitespace stripped
  //   - `language=<value>` => `text/<value>` (no trimming)
  //
  // Notably, whitespace-only values do *not* count as empty-string defaults.
  let type_value_raw = script.get_attribute_ref("type");
  let language_value_raw = script.get_attribute_ref("language");

  let type_string = if let Some(value) = type_value_raw {
    if value.is_empty() {
      "text/javascript".to_string()
    } else {
      trim_ascii_whitespace(value).to_string()
    }
  } else if let Some(value) = language_value_raw {
    if value.is_empty() {
      "text/javascript".to_string()
    } else {
      format!("text/{}", value)
    }
  } else {
    "text/javascript".to_string()
  };

  // `module` / `importmap` must match exactly (after trimming performed above).
  if type_string.eq_ignore_ascii_case("module") {
    return ScriptType::Module;
  }
  if type_string.eq_ignore_ascii_case("importmap") {
    return ScriptType::ImportMap;
  }

  // JavaScript MIME type essence match (WHATWG MIME Sniffing + HTML).
  let mime_essence = type_string
    .split_once(';')
    .map(|(essence, _)| trim_ascii_whitespace(essence))
    .unwrap_or(type_string.as_str())
    .trim_matches(|c: char| c.is_ascii_whitespace());

  const JS_MIME_TYPE_ESSENCES: [&str; 16] = [
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
  ];
  if JS_MIME_TYPE_ESSENCES
    .iter()
    .any(|ty| mime_essence.eq_ignore_ascii_case(ty))
  {
    return ScriptType::Classic;
  }

  ScriptType::Unknown
}

#[cfg(test)]
mod tests {
  use super::{determine_script_type, ScriptType};
  use crate::dom::{DomNode, DomNodeType};

  fn script(attrs: &[(&str, &str)]) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: "script".to_string(),
        namespace: String::new(),
        attributes: attrs
          .iter()
          .map(|(k, v)| (k.to_string(), v.to_string()))
          .collect(),
      },
      children: Vec::new(),
    }
  }

  #[test]
  fn defaults_to_classic_without_type_or_language() {
    let node = script(&[]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn type_empty_string_defaults_to_classic() {
    let node = script(&[("type", "")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn type_whitespace_does_not_default_and_is_unknown() {
    let node = script(&[("type", "  ")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn language_empty_string_defaults_to_classic_when_no_type() {
    let node = script(&[("language", "")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn language_ecmascript_maps_to_classic() {
    let node = script(&[("language", "ecmascript")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn legacy_javascript_mime_types_map_to_classic() {
    for ty in [
      "text/javascript1.5",
      "text/jscript",
      "text/livescript",
      "text/x-javascript",
      "application/x-javascript",
    ] {
      let node = script(&[("type", ty)]);
      assert_eq!(determine_script_type(&node), ScriptType::Classic, "type={ty}");
    }
  }

  #[test]
  fn module_and_importmap_require_exact_match() {
    let node = script(&[("type", "module")]);
    assert_eq!(determine_script_type(&node), ScriptType::Module);
    let node = script(&[("type", "module; charset=utf-8")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let node = script(&[("type", "importmap")]);
    assert_eq!(determine_script_type(&node), ScriptType::ImportMap);
    let node = script(&[("type", "importmap; foo=bar")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn type_trimming_is_ascii_only() {
    // HTML trims ASCII whitespace, not all Unicode whitespace.
    let nbsp = "\u{00A0}";
    let module_trailing = format!("module{nbsp}");
    let module_wrapped = format!("{nbsp}module{nbsp}");
    let js_trailing = format!("text/javascript{nbsp}");

    let node = script(&[("type", module_trailing.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
    let node = script(&[("type", module_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let node = script(&[("type", js_trailing.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn type_trimming_excludes_vertical_tab() {
    // HTML's ASCII whitespace definition does not include U+000B VT.
    let vt = "\u{000B}";

    let module_wrapped = format!("{vt}module{vt}");
    let node = script(&[("type", module_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let js_wrapped = format!("{vt}text/javascript{vt}");
    let node = script(&[("type", js_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }
}
