use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::InsetValue;
use crate::ComputedStyle;
use crate::Length;
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
fn legacy_offset_inline_start_applies_in_stylesheet() {
  let styles = div_styles(
    "<div></div>",
    "div { position: absolute; offset-inline-start: 10px; }",
  );
  assert_eq!(styles.left, InsetValue::Length(Length::px(10.0)));
}

#[test]
fn legacy_offset_inline_start_applies_in_inline_style_attribute() {
  let styles = div_styles(
    r#"<div style="position:absolute; offset-inline-start: 7px"></div>"#,
    "",
  );
  assert_eq!(styles.left, InsetValue::Length(Length::px(7.0)));
}

#[test]
fn legacy_offset_inline_start_respects_direction_in_rtl() {
  let styles = div_styles(
    "<div></div>",
    "div { position: absolute; direction: rtl; offset-inline-start: 10px; }",
  );
  assert_eq!(styles.right, InsetValue::Length(Length::px(10.0)));
  assert_eq!(styles.left, InsetValue::Auto);
}

#[test]
fn legacy_offset_and_inset_follow_cascade_order_last_wins() {
  let styles = div_styles(
    "<div></div>",
    "div { position: absolute; offset-inline-start: 10px; inset-inline-start: 20px; }",
  );
  assert_eq!(styles.left, InsetValue::Length(Length::px(20.0)));

  let styles = div_styles(
    "<div></div>",
    "div { position: absolute; inset-inline-start: 20px; offset-inline-start: 10px; }",
  );
  assert_eq!(styles.left, InsetValue::Length(Length::px(10.0)));
}

