use fastrender::css::properties::parse_property_value;
use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::PropertyValue;
use fastrender::css::types::Transform;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::AlignContent;
use fastrender::style::types::FilterFunction;
use fastrender::style::types::TextSizeAdjust;
use fastrender::style::types::UserSelect;
use fastrender::ComputedStyle;
use fastrender::Length;
use fastrender::LengthUnit;
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

fn div_transform(html: &str, css: &str) -> Vec<Transform> {
  div_styles(html, css).transform.clone()
}

#[test]
fn vendor_prefixed_declaration_applies_in_stylesheet() {
  let transforms = div_transform("<div></div>", "div { -webkit-transform: rotate(10deg); }");
  assert!(
    matches!(transforms.as_slice(), [Transform::Rotate(angle)] if (*angle - 10.0).abs() < 1e-6),
    "expected rotate(10deg), got {transforms:?}"
  );
}

#[test]
fn vendor_prefixed_declaration_applies_in_inline_style_attribute() {
  let transforms = div_transform(
    r#"<div style="-webkit-transform: rotate(10deg)"></div>"#,
    "",
  );
  assert!(
    matches!(transforms.as_slice(), [Transform::Rotate(angle)] if (*angle - 10.0).abs() < 1e-6),
    "expected rotate(10deg) from inline style, got {transforms:?}"
  );
}

#[test]
fn vendor_prefixed_and_unprefixed_properties_follow_cascade_order_unprefixed_last() {
  let transforms = div_transform(
    "<div></div>",
    "div { -webkit-transform: rotate(10deg); transform: rotate(20deg); }",
  );
  assert!(
    matches!(transforms.as_slice(), [Transform::Rotate(angle)] if (*angle - 20.0).abs() < 1e-6),
    "expected unprefixed declaration to win, got {transforms:?}"
  );
}

#[test]
fn vendor_prefixed_and_unprefixed_properties_follow_cascade_order_prefixed_last() {
  let transforms = div_transform(
    "<div></div>",
    "div { transform: rotate(20deg); -webkit-transform: rotate(10deg); }",
  );
  assert!(
    matches!(transforms.as_slice(), [Transform::Rotate(angle)] if (*angle - 10.0).abs() < 1e-6),
    "expected prefixed alias to win when last, got {transforms:?}"
  );
}

#[test]
fn vendor_prefixed_properties_are_case_insensitive() {
  let transforms = div_transform(
    r#"<div style="-WeBkIt-TrAnSfOrM: rotate(10deg)"></div>"#,
    "",
  );
  assert!(
    matches!(transforms.as_slice(), [Transform::Rotate(angle)] if (*angle - 10.0).abs() < 1e-6),
    "expected case-insensitive -webkit-transform, got {transforms:?}"
  );
}

#[test]
fn vendor_prefixed_backdrop_filter_applies() {
  let styles = div_styles("<div></div>", "div { -webkit-backdrop-filter: blur(5px); }");
  assert!(
    matches!(styles.backdrop_filter.as_slice(), [FilterFunction::Blur(len)] if (len.to_px() - 5.0).abs() < 0.01),
    "expected blur(5px), got {:?}",
    styles.backdrop_filter
  );
}

#[test]
fn vendor_prefixed_user_select_applies() {
  let styles = div_styles("<div></div>", "div { -webkit-user-select: none; }");
  assert_eq!(
    styles.user_select,
    UserSelect::None,
    "expected -webkit-user-select to alias to user-select"
  );
}

#[test]
fn vendor_prefixed_webkit_logical_margin_start_applies() {
  let styles = div_styles("<div></div>", "div { -webkit-margin-start: 10px; }");
  assert_eq!(styles.margin_left, Some(Length::px(10.0)));
}

#[test]
fn vendor_prefixed_webkit_logical_margin_start_respects_direction() {
  let styles = div_styles(
    "<div></div>",
    "div { direction: rtl; -webkit-margin-start: 10px; }",
  );
  assert_eq!(styles.margin_right, Some(Length::px(10.0)));
}

#[test]
fn vendor_prefixed_webkit_logical_padding_end_applies() {
  let styles = div_styles("<div></div>", "div { -webkit-padding-end: 7px; }");
  assert_eq!(styles.padding_right, Length::px(7.0));
}

#[test]
fn vendor_prefixed_ms_flex_line_pack_maps_to_align_content() {
  let styles = div_styles("<div></div>", "div { -ms-flex-line-pack: justify; }");
  assert_eq!(styles.align_content, AlignContent::SpaceBetween);
}

#[test]
fn vendor_prefixed_text_size_adjust_applies() {
  let styles = div_styles("<div></div>", "div { -webkit-text-size-adjust: none; }");
  assert_eq!(styles.text_size_adjust, TextSizeAdjust::None);
}

#[test]
fn parse_property_value_accepts_vendor_prefixed_text_size_adjust() {
  let value = parse_property_value("-webkit-text-size-adjust", "none").expect("value parsed");
  match value {
    PropertyValue::Keyword(kw) => assert!(kw.eq_ignore_ascii_case("none")),
    other => panic!("expected keyword, got {other:?}"),
  }

  let value = parse_property_value("-webkit-text-size-adjust", "125%").expect("value parsed");
  match value {
    PropertyValue::Percentage(pct) => assert!((pct - 125.0).abs() < 1e-6),
    PropertyValue::Length(len) if len.unit == LengthUnit::Percent => {
      assert!((len.value - 125.0).abs() < 1e-6)
    }
    other => panic!("expected percentage, got {other:?}"),
  }
}

#[test]
fn parse_property_value_vendor_prefixed_property_name_is_case_insensitive() {
  let value = parse_property_value("-WeBkIt-TeXt-SiZe-AdJuSt", "none").expect("value parsed");
  match value {
    PropertyValue::Keyword(kw) => assert!(kw.eq_ignore_ascii_case("none")),
    other => panic!("expected keyword, got {other:?}"),
  }
}
