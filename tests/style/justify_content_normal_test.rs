use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::types::JustifyContent;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn justify_content_normal_overrides_previous_declarations() {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      #t { display: flex; justify-content: center; }
      #t { justify-content: normal; }
    "#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let target = find_by_id(&styled, "t").expect("element with id t");
  assert_eq!(target.styles.justify_content, JustifyContent::FlexStart);
}

#[test]
fn supports_declaration_accepts_justify_content_normal() {
  assert!(fastrender::css::supports::supports_declaration(
    "justify-content",
    "normal"
  ));
}

