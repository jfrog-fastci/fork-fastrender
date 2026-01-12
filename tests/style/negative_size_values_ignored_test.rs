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
fn negative_width_is_ignored() {
  let node = styled_div(r#"<div style="width:-1px"></div>"#);
  assert_eq!(node.styles.width, None);
}

#[test]
fn negative_height_is_ignored() {
  let node = styled_div(r#"<div style="height:-1px"></div>"#);
  assert_eq!(node.styles.height, None);
}

#[test]
fn negative_width_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="width:10px; width:-1px"></div>"#);
  assert_eq!(node.styles.width, Some(Length::px(10.0)));
}

#[test]
fn negative_height_does_not_override_prior_valid_value() {
  let node = styled_div(r#"<div style="height:10px; height:-1px"></div>"#);
  assert_eq!(node.styles.height, Some(Length::px(10.0)));
}

#[test]
fn negative_inline_size_does_not_override_width() {
  let node = styled_div(r#"<div style="width:10px; inline-size:-1px"></div>"#);
  assert_eq!(node.styles.width, Some(Length::px(10.0)));
}

#[test]
fn negative_block_size_does_not_override_height() {
  let node = styled_div(r#"<div style="height:10px; block-size:-1px"></div>"#);
  assert_eq!(node.styles.height, Some(Length::px(10.0)));
}
