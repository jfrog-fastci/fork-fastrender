use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::TextOverflowSide;

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
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet("").expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  find_first(&styled, "div").expect("div").clone()
}

#[test]
fn invalid_text_overflow_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="text-overflow: ellipsis; text-overflow: ellipsis foo;"></div>"#);
  assert!(matches!(
    node.styles.text_overflow.inline_start,
    TextOverflowSide::Ellipsis
  ));
  assert!(matches!(
    node.styles.text_overflow.inline_end,
    TextOverflowSide::Ellipsis
  ));
}

