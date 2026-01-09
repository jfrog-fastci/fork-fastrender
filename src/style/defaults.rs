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

  // Handle percentage like "85%"
  if dim_str.ends_with('%') {
    if let Ok(value) =
      trim_ascii_whitespace_html(&dim_str[..dim_str.len() - 1]).parse::<f32>()
    {
      return Some(Length::percent(value));
    }
  }

  // Handle pixels (just a number like "18")
  if let Ok(value) = dim_str.parse::<f32>() {
    return Some(Length::px(value));
  }

  None
}

/// Parse HTML bgcolor attribute
///
/// Handles hex colors like #ff6600 or ff6600, with 3 or 6 digit variants.
pub fn parse_color_attribute(color_str: &str) -> Option<Rgba> {
  let color_str = trim_ascii_whitespace_html(color_str);

  // Handle hex colors like #ff6600 or ff6600
  if color_str.starts_with('#') {
    let hex = &color_str[1..];
    if hex.len() == 6 {
      if let (Ok(r), Ok(g), Ok(b)) = (
        u8::from_str_radix(&hex[0..2], 16),
        u8::from_str_radix(&hex[2..4], 16),
        u8::from_str_radix(&hex[4..6], 16),
      ) {
        return Some(Rgba { r, g, b, a: 1.0 });
      }
    } else if hex.len() == 3 {
      // Shorthand like #f60
      if let (Ok(r), Ok(g), Ok(b)) = (
        u8::from_str_radix(&hex[0..1], 16),
        u8::from_str_radix(&hex[1..2], 16),
        u8::from_str_radix(&hex[2..3], 16),
      ) {
        // Double each digit: #f60 -> #ff6600
        return Some(Rgba {
          r: r * 17,
          g: g * 17,
          b: b * 17,
          a: 1.0,
        });
      }
    }
  }

  // Fallback to CSS color parsing for rgb()/named colors.
  if let Ok(color) = crate::style::color::Color::parse(color_str) {
    return Some(color.to_rgba(Rgba::BLACK));
  }

  None
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
  fn non_ascii_whitespace_parse_color_attribute_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert!(
      parse_color_attribute(&format!("{nbsp}ff6600")).is_none(),
      "NBSP must not be treated as ASCII whitespace"
    );
    assert!(
      parse_color_attribute(&format!("{nbsp}#ff6600")).is_none(),
      "NBSP must not be treated as ASCII whitespace"
    );
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
