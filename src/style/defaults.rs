//! Default computed style seed values.
//!
//! `ComputedStyle::default()` represents the CSS initial values and is used as the base for the
//! cascade.
//!
//! Element-specific user-agent defaults (e.g. `body { margin: 8px }`, `h1` sizing, table/list
//! display types, etc.) live in [`src/user_agent.css`](../../user_agent.css) and are applied via the
//! normal cascade as UA origin rules.
//!
//! This module is intentionally limited to defaults that cannot be expressed as UA CSS (internal
//! nodes like the document and shadow roots) and a small number of compatibility/presentational
//! mappings that are implemented in Rust.

use crate::debug::runtime::runtime_toggles;
use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::style::ComputedStyle;
use crate::style::Display;
use crate::style::Length;
use crate::style::Rgba;
use std::sync::OnceLock;

static DEFAULT_COMPUTED_STYLE: OnceLock<ComputedStyle> = OnceLock::new();

const ENV_COMPAT_REPLACED_MAX_WIDTH_100: &str = "FASTR_COMPAT_REPLACED_MAX_WIDTH_100";

fn default_computed_style() -> &'static ComputedStyle {
  DEFAULT_COMPUTED_STYLE.get_or_init(ComputedStyle::default)
}

/// Returns the initial computed style for a node.
///
/// This is the seed style used before applying the UA and author stylesheets.
///
/// Policy:
/// - Element-specific UA defaults MUST be expressed in `src/user_agent.css`.
/// - This function should only contain:
///   - defaults for non-element/internal node types (document/shadow root)
///   - non-CSS defaults needed by the engine (e.g. `<legend>` sizing)
///   - opt-in compatibility toggles that are intentionally not part of the UA stylesheet
pub fn get_default_styles_for_element(node: &DomNode) -> ComputedStyle {
  let mut styles = default_computed_style().clone();

  // Handle Document/shadow root node types - they act as containers only.
  if matches!(node.node_type, DomNodeType::Document { .. }) {
    // The document node itself does not generate a CSS box; only the root element does.
    // Treating it as `display: contents` prevents an extra anonymous wrapper box from
    // interfering with writing-mode-aware layout and fragmentation (paged media, multi-column
    // fragmentation, etc).
    styles.display = Display::Contents;
    return styles;
  }
  if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
    styles.display = Display::Contents;
    return styles;
  }

  if let Some(tag) = node.tag_name() {
    match tag {
      "legend" => {
        styles.shrink_to_fit_inline_size = true;
      }
      "img" | "video" | "audio" | "canvas" | "svg" | "iframe" | "embed" | "object" => {
        // Compatibility default (non-standard): `max-width: 100%` on replaced elements.
        //
        // This is *not* a standard UA default, but many "responsive" pages rely on author CSS that
        // effectively does the same thing (e.g. `img { max-width: 100%; height: auto; }`). Keeping
        // this behavior behind a runtime toggle makes it possible to A/B against Chrome/pageset
        // fixtures without silently masking missing author styles.
        if runtime_toggles().truthy_with_default(ENV_COMPAT_REPLACED_MAX_WIDTH_100, true) {
          styles.max_width = Some(Length::percent(100.0));
          styles.max_width_keyword = None;
        }
      }
      "math" => {
        // Prefer math fonts when available and honor display="block" attribute
        styles.font_family = vec!["math".to_string(), "serif".to_string()].into();
        if let Some(display) = node.get_attribute("display") {
          if display.eq_ignore_ascii_case("block") {
            styles.display = Display::Block;
          }
        }
      }
      _ => {}
    }
  }

  styles
}

#[inline]
fn is_ascii_whitespace_html(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace_html(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html)
}

/// Parse HTML width/height attribute
///
/// Handles both percentage values like "85%" and pixel values like "18".
pub fn parse_dimension_attribute(dim_str: &str) -> Option<Length> {
  let dim_str = trim_ascii_whitespace_html(dim_str);

  if dim_str.is_empty() {
    return None;
  }

  fn parse_number_prefix(s: &str) -> Option<f32> {
    let s = trim_ascii_whitespace_html(s);
    if s.is_empty() {
      return None;
    }

    // HTML "dimension attribute" parsing in the wild commonly accepts a `px` suffix (e.g. `90px`)
    // even though the content attribute is defined as a number. Browsers parse the numeric prefix
    // and ignore the rest; do the same for presentational hints.
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut saw_digit = false;
    while i < bytes.len() {
      let b = bytes[i];
      if b.is_ascii_digit() {
        saw_digit = true;
        i += 1;
        continue;
      }
      if b == b'.' {
        i += 1;
        continue;
      }
      break;
    }

    if !saw_digit || i == 0 {
      return None;
    }

    s[..i].parse::<f32>().ok()
  }

  // Handle percentage like "85%"
  if dim_str.ends_with('%') {
    let raw = trim_ascii_whitespace_html(&dim_str[..dim_str.len() - 1]);
    let value = raw.parse::<f32>().ok().or_else(|| parse_number_prefix(raw))?;
    if value.is_finite() && value >= 0.0 {
      return Some(Length::percent(value));
    }
    return None;
  }

  // Handle pixels (just a number like "18")
  if let Ok(value) = dim_str.parse::<f32>() {
    if value.is_finite() && value >= 0.0 {
      return Some(Length::px(value));
    }
    return None;
  }

  let value = parse_number_prefix(dim_str)?;
  (value.is_finite() && value >= 0.0).then_some(Length::px(value))
}

/// Parse HTML bgcolor attribute
///
/// Implements the WHATWG HTML "rules for parsing a legacy color value".
///
/// References:
/// - WHATWG HTML: `#rules-for-parsing-a-legacy-colour-value`
pub fn parse_color_attribute(color_str: &str) -> Option<Rgba> {
  // Step 1: If input is the empty string, return failure.
  if color_str.is_empty() {
    return None;
  }

  // Step 2: Strip leading/trailing ASCII whitespace.
  let trimmed = trim_ascii_whitespace_html(color_str);

  // Step 3: If input is "transparent" (ASCII case-insensitive), return failure.
  if trimmed.eq_ignore_ascii_case("transparent") {
    return None;
  }

  // Step 4: If input is a named color, return it (CSS2 system colors are *not* recognized).
  if let Some(named) = crate::style::color::parse_named_color(trimmed) {
    return Some(named);
  }

  // Step 5: Special-case "#rgb" shorthand (exactly 4 code points: "#"+3 hex digits).
  if trimmed.starts_with('#') && trimmed.chars().count() == 4 {
    let mut chars = trimmed.chars();
    let _hash = chars.next();
    let r = chars.next()?;
    let g = chars.next()?;
    let b = chars.next()?;
    if r.is_ascii_hexdigit() && g.is_ascii_hexdigit() && b.is_ascii_hexdigit() {
      let to_nibble = |c: char| c.to_digit(16).map(|v| v as u8);
      let r = to_nibble(r)?;
      let g = to_nibble(g)?;
      let b = to_nibble(b)?;
      return Some(Rgba::rgb(r * 17, g * 17, b * 17));
    }
  }

  // Steps 6-7: Replace non-BMP code points with "00", then truncate to 128 code points.
  // (We do both in one pass.)
  let mut input = String::new();
  let mut codepoints = 0usize;
  for ch in trimmed.chars() {
    if codepoints >= 128 {
      break;
    }
    if (ch as u32) > 0xFFFF {
      // Replace with "00" (two code points), but respect the 128-code-point cap.
      if codepoints >= 127 {
        input.push('0');
        break;
      }
      input.push('0');
      input.push('0');
      codepoints += 2;
    } else {
      input.push(ch);
      codepoints += 1;
    }
  }

  // Step 8: If first character is '#', remove it.
  if input.starts_with('#') {
    input.remove(0);
  }

  // Step 9: Replace any non-ASCII-hex-digit character with '0'.
  input = input
    .chars()
    .map(|c| if c.is_ascii_hexdigit() { c } else { '0' })
    .collect();

  // Step 10: While length is zero or not a multiple of 3, append '0'.
  while input.is_empty() || input.len() % 3 != 0 {
    input.push('0');
  }

  // Steps 11+: Split into 3 equal components.
  let component_len = input.len() / 3;
  let (mut r_part, rest) = input.split_at(component_len);
  let (mut g_part, mut b_part) = rest.split_at(component_len);

  // Step 12: If component length > 8, drop leading (len-8) chars in each component.
  let mut len = component_len;
  if len > 8 {
    let drop = len - 8;
    r_part = &r_part[drop..];
    g_part = &g_part[drop..];
    b_part = &b_part[drop..];
    len = 8;
  }

  // Step 13: While len > 2 and first char of each component is '0', drop first char.
  while len > 2
    && r_part.as_bytes().first() == Some(&b'0')
    && g_part.as_bytes().first() == Some(&b'0')
    && b_part.as_bytes().first() == Some(&b'0')
  {
    r_part = &r_part[1..];
    g_part = &g_part[1..];
    b_part = &b_part[1..];
    len -= 1;
  }

  // Step 14: If len is still > 2, truncate each component to its first 2 chars.
  if len > 2 {
    r_part = &r_part[..2];
    g_part = &g_part[..2];
    b_part = &b_part[..2];
  }

  // Steps 15-17: Interpret each component as a hex number.
  let r = u8::from_str_radix(r_part, 16).ok()?;
  let g = u8::from_str_radix(g_part, 16).ok()?;
  let b = u8::from_str_radix(b_part, 16).ok()?;
  Some(Rgba::rgb(r, g, b))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{set_runtime_toggles, RuntimeToggles};
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
  use std::collections::HashMap;
  use std::sync::Arc;

  fn collect_embed_object_widths(node: &FragmentNode, embeds: &mut Vec<f32>, objects: &mut Vec<f32>) {
    if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
      match replaced_type {
        ReplacedType::Embed { .. } => embeds.push(node.bounds.width()),
        ReplacedType::Object { .. } => objects.push(node.bounds.width()),
        _ => {}
      }
    }
    for child in node.children.iter() {
      collect_embed_object_widths(child, embeds, objects);
    }
  }

  #[test]
  fn non_ascii_whitespace_parse_dimension_attribute_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert!(
      parse_dimension_attribute(&format!("{nbsp}85%")).is_none(),
      "NBSP must not be treated as ASCII whitespace"
    );
    assert!(
      parse_dimension_attribute(&format!("{nbsp}18")).is_none(),
      "NBSP must not be treated as ASCII whitespace"
    );
  }

  #[test]
  fn parse_dimension_attribute_accepts_px_suffix() {
    assert_eq!(parse_dimension_attribute("90px"), Some(Length::px(90.0)));
    assert_eq!(parse_dimension_attribute("  50px "), Some(Length::px(50.0)));
    assert_eq!(parse_dimension_attribute("  50PX  "), Some(Length::px(50.0)));
    assert_eq!(parse_dimension_attribute("85%"), Some(Length::percent(85.0)));
  }

  #[test]
  fn non_ascii_whitespace_parse_color_attribute_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert_ne!(
      parse_color_attribute(&format!("{nbsp}ff6600")),
      parse_color_attribute("ff6600"),
      "NBSP must not be treated as ASCII whitespace"
    );
    assert_ne!(
      parse_color_attribute(&format!("{nbsp}#ff6600")),
      parse_color_attribute("#ff6600"),
      "NBSP must not be treated as ASCII whitespace"
    );
  }

  #[test]
  fn parse_color_attribute_parses_hashless_hex() {
    assert_eq!(parse_color_attribute("ff6600"), Some(Rgba::rgb(255, 102, 0)));
    assert_eq!(
      parse_color_attribute("  ff6600  "),
      Some(Rgba::rgb(255, 102, 0))
    );
  }

  #[test]
  fn parse_color_attribute_rejects_transparent() {
    assert!(parse_color_attribute("transparent").is_none());
    assert!(parse_color_attribute("Transparent").is_none());
  }

  #[test]
  fn parse_color_attribute_does_not_recognize_system_colors() {
    // HTML legacy color parsing only recognizes named colors, not CSS system color keywords like
    // "Canvas". Treat it as a "hashless hex" legacy color value instead.
    assert_eq!(parse_color_attribute("Canvas"), Some(Rgba::rgb(202, 0, 160)));
  }

  fn layout_widths(toggle: &str) -> (Vec<f32>, Vec<f32>) {
    let _guard = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::from([(
      ENV_COMPAT_REPLACED_MAX_WIDTH_100.to_string(),
      toggle.to_string(),
    )]))));

    let html = r#"
      <html>
        <head><style>body { margin: 0; }</style></head>
        <body>
          <embed src="about:blank" width="300" height="10">
          <object data="about:blank" width="300" height="10"></object>
        </body>
      </html>
    "#;
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled =
      crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = crate::tree::box_generation::generate_box_tree_with_anonymous_fixup(&styled)
      .expect("box tree");

    let engine =
      crate::layout::engine::LayoutEngine::new(crate::layout::engine::LayoutConfig::for_viewport(
        crate::geometry::Size::new(100.0, 100.0),
      ));
    let fragment_tree = engine.layout_tree(&box_tree).expect("layout");

    let mut embeds = Vec::new();
    let mut objects = Vec::new();
    collect_embed_object_widths(&fragment_tree.root, &mut embeds, &mut objects);
    (embeds, objects)
  }

  #[test]
  fn compat_replaced_max_width_clamps_embed_and_object() {
    let (embeds_off, objects_off) = layout_widths("0");
    assert_eq!(embeds_off.len(), 1, "expected one <embed> fragment");
    assert_eq!(objects_off.len(), 1, "expected one <object> fragment");
    assert!(
      embeds_off[0] > 100.0,
      "expected <embed> to overflow without compat max-width, got {}",
      embeds_off[0]
    );
    assert!(
      objects_off[0] > 100.0,
      "expected <object> to overflow without compat max-width, got {}",
      objects_off[0]
    );

    let (embeds_on, objects_on) = layout_widths("1");
    assert_eq!(embeds_on.len(), 1, "expected one <embed> fragment");
    assert_eq!(objects_on.len(), 1, "expected one <object> fragment");
    assert!(
      embeds_on[0] <= 100.01,
      "expected <embed> to be clamped to the viewport with compat max-width, got {}",
      embeds_on[0]
    );
    assert!(
      objects_on[0] <= 100.01,
      "expected <object> to be clamped to the viewport with compat max-width, got {}",
      objects_on[0]
    );
  }
}
