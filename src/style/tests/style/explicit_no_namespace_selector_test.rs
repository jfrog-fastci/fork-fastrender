use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::computed::Visibility;
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

#[test]
fn explicit_no_namespace_type_selectors_do_not_match_svg_elements() {
  let html = r#"
    <div id="d"></div>
    <svg><rect id="r"></rect></svg>
  "#;
  let dom = dom::parse_html(html).unwrap();
  // `|E` is an explicit no-namespace type selector. It must not match SVG elements.
  let css = r#"
    |rect { display: block; visibility: hidden; }
    rect { display: inline; }
    div { display: flex; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let rect = find_by_id(&styled, "r").expect("svg rect");
  assert_eq!(display(rect), "inline");
  assert_eq!(rect.styles.visibility, Visibility::Visible);
  assert_eq!(display(find_by_id(&styled, "d").expect("html div")), "flex");
}
