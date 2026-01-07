use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
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
fn nth_child_of_argument_specificity_beats_source_order() {
  let html = r#"
    <ul>
      <li id="target"></li>
      <li></li>
    </ul>
  "#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    li { display: block; }
    li:nth-child(1 of #target) { display: inline; }
    li:nth-child(1) { display: inline-block; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "target").expect("target")),
    "inline"
  );
}

#[test]
fn nth_last_child_of_argument_specificity_beats_source_order() {
  let html = r#"
    <ul>
      <li></li>
      <li id="target"></li>
    </ul>
  "#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    li { display: block; }
    li:nth-last-child(1 of #target) { display: inline; }
    li:nth-last-child(1) { display: inline-block; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "target").expect("target")),
    "inline"
  );
}

