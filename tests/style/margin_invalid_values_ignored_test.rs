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
fn invalid_margin_top_value_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="margin-top: 10px; margin-top: foo;"></div>"#);
  assert_eq!(node.styles.margin_top, Some(Length::px(10.0)));
}

#[test]
fn invalid_margin_shorthand_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="margin: 5px; margin: 10px foo;"></div>"#);
  assert_eq!(node.styles.margin_top, Some(Length::px(5.0)));
  assert_eq!(node.styles.margin_right, Some(Length::px(5.0)));
  assert_eq!(node.styles.margin_bottom, Some(Length::px(5.0)));
  assert_eq!(node.styles.margin_left, Some(Length::px(5.0)));
}

#[test]
fn invalid_margin_inline_does_not_override_prior_valid_value() {
  let node = styled_div(
    r#"<div style="margin-inline: 7px; margin-inline: 1px 2px 3px;"></div>"#,
  );
  assert_eq!(node.styles.margin_left, Some(Length::px(7.0)));
  assert_eq!(node.styles.margin_right, Some(Length::px(7.0)));
}

