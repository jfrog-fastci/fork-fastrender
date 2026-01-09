use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

fn div_styles(html: &str, css: &str) -> Arc<ComputedStyle> {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  Arc::clone(&div.styles)
}

#[test]
fn webkit_text_stroke_shorthand_sets_width_and_color() {
  let styles = div_styles("<div>Hello</div>", "div { -webkit-text-stroke: 2px red; }");
  assert!(
    (styles.webkit_text_stroke_width.to_px() - 2.0).abs() < 1e-6,
    "expected 2px, got {:?}",
    styles.webkit_text_stroke_width
  );
  assert_eq!(
    styles.webkit_text_stroke_color.to_rgba(styles.color),
    Rgba::RED,
    "expected stroke color to resolve to red"
  );
}

#[test]
fn webkit_text_stroke_shorthand_resets_missing_components_to_initial() {
  let styles = div_styles(
    "<div>Hello</div>",
    "div { -webkit-text-stroke-width: 1px; -webkit-text-stroke: red; }",
  );
  assert!(
    styles.webkit_text_stroke_width.to_px().abs() < 1e-6,
    "expected shorthand to reset width to initial (0px), got {:?}",
    styles.webkit_text_stroke_width
  );
  assert_eq!(
    styles.webkit_text_stroke_color.to_rgba(styles.color),
    Rgba::RED,
    "expected shorthand to set stroke color"
  );
}

#[test]
fn webkit_text_stroke_width_negative_value_is_ignored() {
  let styles = div_styles(
    "<div>Hello</div>",
    "div { -webkit-text-stroke-width: 1px; -webkit-text-stroke-width: -2px; }",
  );
  assert!(
    (styles.webkit_text_stroke_width.to_px() - 1.0).abs() < 1e-6,
    "expected negative width to be ignored, got {:?}",
    styles.webkit_text_stroke_width
  );
}

