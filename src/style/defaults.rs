//! Default styles for HTML elements
//!
//! Provides initial/default computed style values for each HTML element type.
//! These are applied before author styles in the cascade.
//!
//! Reference: HTML5 Living Standard - Rendering
//! <https://html.spec.whatwg.org/multipage/rendering.html>

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

/// Get default styles for an HTML element
///
/// Returns a ComputedStyle with appropriate default values for the given element.
/// These defaults generally match the user-agent stylesheet behavior for HTML elements.
/// Some non-standard compatibility defaults are gated behind `FASTR_*` runtime toggles so we can
/// A/B them against Chrome/pageset fixtures.
///
/// Note: All styling should come from CSS (user-agent.css or author styles),
/// not from class-name checks in Rust code. This function only sets tag-based defaults.
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

  // Set proper default display values for HTML elements (user-agent stylesheet defaults)
  if let Some(tag) = node.tag_name() {
    styles.display = match tag {
      // Document structure elements (must be block to establish formatting context)
      "html" | "body" => Display::Block,

      // Block-level elements
      "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "blockquote"
      | "pre" | "article" | "section" | "nav" | "aside" | "header" | "footer" | "main"
      | "figure" | "figcaption" | "dl" | "dt" | "dd" | "form" | "fieldset" | "legend"
      | "address" | "hr" => Display::Block,

      // Lists
      "li" => Display::ListItem,

      // Center element - centers its contents
      "center" => Display::Block,

      // Table elements
      "table" => Display::Table,
      "caption" => Display::TableCaption,
      "colgroup" => Display::TableColumnGroup,
      "col" => Display::TableColumn,
      "tr" => Display::TableRow,
      "td" | "th" => Display::TableCell,
      "thead" | "tbody" | "tfoot" => Display::TableRowGroup,

      // Inline elements (explicit for clarity, though it's the default)
      "a" | "span" | "em" | "strong" | "code" | "b" | "i" | "u" | "small" | "sub" | "sup"
      | "mark" | "abbr" | "cite" | "q" | "kbd" | "samp" | "var" | "time" | "label" => {
        Display::Inline
      }

      // Replaced elements: keep them inline by default
      "img" | "video" | "audio" | "canvas" | "svg" | "math" => Display::Inline,

      // Hidden elements (display: none - not rendered)
      "head" | "style" | "script" | "meta" | "link" | "title" | "template" => Display::None,

      // Everything else defaults to inline
      _ => Display::Inline,
    };

    // Force minimal spacing for table elements (consistent with user-agent.css)
    match tag {
      "body" => {
        // UA default margins: 8px on all sides
        let default_margin = Some(Length::px(8.0));
        styles.margin_top = default_margin;
        styles.margin_right = default_margin;
        styles.margin_bottom = default_margin;
        styles.margin_left = default_margin;
      }
      "table" => {
        // Remove all spacing from tables
        styles.margin_top = Some(Length::px(0.0));
        styles.margin_bottom = Some(Length::px(0.0));
        styles.padding_top = Length::px(0.0);
        styles.padding_bottom = Length::px(0.0);
        // UA default border-spacing per HTML/CSS UA stylesheets (CSS2 §17.6.1).
        styles.border_spacing_horizontal = Length::px(2.0);
        styles.border_spacing_vertical = Length::px(2.0);
      }
      "tr" => {
        // Minimal spacing between table rows
        styles.margin_top = Some(Length::px(0.0));
        styles.margin_bottom = Some(Length::px(0.0));
        styles.padding_top = Length::px(0.0);
        styles.padding_bottom = Length::px(0.0);
      }
      "td" => {
        // Minimal padding for table cells
        styles.padding_top = Length::px(1.0);
        styles.padding_bottom = Length::px(1.0);
        styles.padding_left = Length::px(1.0);
        styles.padding_right = Length::px(1.0);
        styles.margin_top = Some(Length::px(0.0));
        styles.margin_bottom = Some(Length::px(0.0));
        // CSS 2.1 §17.5.3: table cells default to middle alignment
        styles.vertical_align = crate::style::types::VerticalAlign::Middle;
      }
      "legend" => {
        styles.shrink_to_fit_inline_size = true;
      }
      "th" => {
        // Header cells inherit td defaults plus bold/centered text
        styles.padding_top = Length::px(1.0);
        styles.padding_bottom = Length::px(1.0);
        styles.padding_left = Length::px(1.0);
        styles.padding_right = Length::px(1.0);
        styles.margin_top = Some(Length::px(0.0));
        styles.margin_bottom = Some(Length::px(0.0));
        styles.vertical_align = crate::style::types::VerticalAlign::Middle;
        styles.text_align = crate::style::types::TextAlign::Center;
        styles.font_weight = crate::style::FontWeight::Bold;
      }
      "b" | "strong" => {
        // Bold text
        styles.font_weight = crate::style::FontWeight::Bold;
      }
      "i" | "em" => {
        // Italic text - using Oblique since we may not have true italics
        styles.font_style = crate::style::FontStyle::Oblique(None);
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
      "slot" => {
        styles.display = Display::Contents;
      }
      _ => {}
    }
  }

  styles
}

/// Parse HTML width/height attribute
///
/// Handles both percentage values like "85%" and pixel values like "18".
pub fn parse_dimension_attribute(dim_str: &str) -> Option<Length> {
  let dim_str = dim_str.trim();

  // Handle percentage like "85%"
  if dim_str.ends_with('%') {
    if let Ok(value) = dim_str[..dim_str.len() - 1].trim().parse::<f32>() {
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
  let color_str = color_str.trim();

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
