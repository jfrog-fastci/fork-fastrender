use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::float::Float;
use crate::style::media::MediaContext;
use crate::style::types::BorderStyle;
use crate::style::types::CaseTransform;
use crate::style::types::TextTransform;
use crate::style::values::Length;
use crate::ComputedStyle;
use crate::Rgba;

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

fn styled_paragraph(css: &str, html: &str) -> StyledNode {
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, "p")
    .cloned()
    .expect("paragraph present")
}

#[test]
fn first_line_filters_non_applicable_properties() {
  let rules =
    "p::first-line { color: rgb(10,20,30); width: 40px; background-color: rgb(200, 210, 220); }";
  let p = styled_paragraph(rules, "<p>hello world</p>");
  let defaults = ComputedStyle::default();
  let line_style = p.first_line_styles.as_ref().expect("first-line styles");

  assert_eq!(line_style.color, Rgba::rgb(10, 20, 30));
  assert_eq!(line_style.background_color, Rgba::rgb(200, 210, 220));
  // Box properties like width should be ignored for ::first-line.
  assert_eq!(line_style.width, defaults.width);
}

#[test]
fn first_line_allows_background_shorthand() {
  let rules = "p::first-line { background: rgb(200, 210, 220); }";
  let p = styled_paragraph(rules, "<p>hello world</p>");
  let line_style = p.first_line_styles.as_ref().expect("first-line styles");
  assert_eq!(line_style.background_color, Rgba::rgb(200, 210, 220));
}

#[test]
fn first_letter_inherits_first_line_and_keeps_box_properties() {
  let rules = "p::first-line { text-transform: uppercase; } p::first-letter { float: left; padding-right: 4px; margin-right: 6px; color: rgb(1,2,3); }";
  let p = styled_paragraph(rules, "<p>hello</p>");
  let letter_style = p.first_letter_styles.as_ref().expect("first-letter styles");

  assert_eq!(letter_style.float, Float::Left);
  assert_eq!(letter_style.padding_right, Length::px(4.0));
  assert_eq!(letter_style.margin_right, Some(Length::px(6.0)));
  // ::first-letter should inherit ::first-line text transforms while honoring its own declarations.
  assert_eq!(
    letter_style.text_transform,
    TextTransform::with_case(CaseTransform::Uppercase)
  );
  assert_eq!(letter_style.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn first_letter_allows_border_shorthand() {
  let rules = "p::first-letter { border: 2px solid rgb(9, 8, 7); }";
  let p = styled_paragraph(rules, "<p>hello</p>");
  let letter_style = p.first_letter_styles.as_ref().expect("first-letter styles");

  assert_eq!(letter_style.border_top_width, Length::px(2.0));
  assert_eq!(letter_style.border_right_width, Length::px(2.0));
  assert_eq!(letter_style.border_bottom_width, Length::px(2.0));
  assert_eq!(letter_style.border_left_width, Length::px(2.0));
  assert_eq!(letter_style.border_top_style, BorderStyle::Solid);
  assert_eq!(letter_style.border_right_style, BorderStyle::Solid);
  assert_eq!(letter_style.border_bottom_style, BorderStyle::Solid);
  assert_eq!(letter_style.border_left_style, BorderStyle::Solid);
  assert_eq!(letter_style.border_top_color, Rgba::rgb(9, 8, 7));
  assert_eq!(letter_style.border_right_color, Rgba::rgb(9, 8, 7));
  assert_eq!(letter_style.border_bottom_color, Rgba::rgb(9, 8, 7));
  assert_eq!(letter_style.border_left_color, Rgba::rgb(9, 8, 7));
}
