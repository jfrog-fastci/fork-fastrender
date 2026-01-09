use std::collections::BTreeMap;

/// Parse an inline `style=""` attribute into a normalized map of declarations.
///
/// This is intentionally a *very* small parser:
/// - Splits on `;` and then the first `:`.
/// - Trims ASCII whitespace around names/values.
/// - Normalizes non-custom property names to lowercase (CSS properties are ASCII-case-insensitive).
/// - Preserves custom property names (`--foo`) verbatim.
///
/// It is sufficient for DOM `element.style.*` shims where we only need to round-trip common
/// properties used by real-world bootstrap scripts.
pub(super) fn parse_style_attribute(value: &str) -> BTreeMap<String, String> {
  let mut out: BTreeMap<String, String> = BTreeMap::new();

  for decl in value.split(';') {
    let decl = decl.trim();
    if decl.is_empty() {
      continue;
    }

    let Some((name, raw_value)) = decl.split_once(':') else {
      continue;
    };
    let name = name.trim();
    if name.is_empty() {
      continue;
    }
    let raw_value = raw_value.trim();

    let name = if name.starts_with("--") {
      name.to_string()
    } else {
      name.to_ascii_lowercase()
    };

    out.insert(name, raw_value.to_string());
  }

  out
}

pub(super) fn serialize_style_attribute(decls: &BTreeMap<String, String>) -> String {
  let mut out = String::new();
  for (idx, (name, value)) in decls.iter().enumerate() {
    if idx > 0 {
      out.push_str("; ");
    }
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
  }
  if !out.is_empty() {
    out.push(';');
  }
  out
}

