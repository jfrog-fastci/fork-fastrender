use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::values::Length;

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

fn styled_div(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, "div").expect("div").clone()
}

#[test]
fn invalid_padding_shorthand_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="padding: 5px; padding: 10px foo;"></div>"#);
  assert_eq!(node.styles.padding_top, Length::px(5.0));
  assert_eq!(node.styles.padding_right, Length::px(5.0));
  assert_eq!(node.styles.padding_bottom, Length::px(5.0));
  assert_eq!(node.styles.padding_left, Length::px(5.0));
}

#[test]
fn invalid_padding_inline_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="padding-inline: 7px; padding-inline: 1px foo;"></div>"#);
  assert_eq!(node.styles.padding_left, Length::px(7.0));
  assert_eq!(node.styles.padding_right, Length::px(7.0));
}

#[test]
fn invalid_border_width_shorthand_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="border-width: 3px; border-width: 1px foo;"></div>"#);
  assert_eq!(node.styles.border_top_width, Length::px(3.0));
  assert_eq!(node.styles.border_right_width, Length::px(3.0));
  assert_eq!(node.styles.border_bottom_width, Length::px(3.0));
  assert_eq!(node.styles.border_left_width, Length::px(3.0));
}

#[test]
fn border_spacing_rejects_extra_tokens() {
  let node = styled_div(r#"<div style="border-spacing: 1px 2px; border-spacing: 3px 4px 5px;"></div>"#);
  assert_eq!(node.styles.border_spacing_horizontal, Length::px(1.0));
  assert_eq!(node.styles.border_spacing_vertical, Length::px(2.0));
}

