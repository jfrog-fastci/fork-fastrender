use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::TextOverflowSide;

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
fn invalid_second_token_does_not_override_text_overflow() {
  let node = styled_div(r#"<div style="text-overflow: clip; text-overflow: ellipsis foo;"></div>"#);
  assert_eq!(
    node.styles.text_overflow.inline_start,
    TextOverflowSide::Clip
  );
  assert_eq!(node.styles.text_overflow.inline_end, TextOverflowSide::Clip);
}

#[test]
fn one_value_text_overflow_applies_to_inline_end_only() {
  let node = styled_div(r#"<div style="text-overflow: ellipsis;"></div>"#);
  assert_eq!(node.styles.text_overflow.inline_start, TextOverflowSide::Clip);
  assert_eq!(node.styles.text_overflow.inline_end, TextOverflowSide::Ellipsis);
}

#[test]
fn text_overflow_accepts_custom_string() {
  let node = styled_div(r#"<div style="text-overflow: '>>';"></div>"#);
  assert_eq!(node.styles.text_overflow.inline_start, TextOverflowSide::Clip);
  assert_eq!(
    node.styles.text_overflow.inline_end,
    TextOverflowSide::String(">>".to_string())
  );
}
