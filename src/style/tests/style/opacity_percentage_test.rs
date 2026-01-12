use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;

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

#[test]
fn opacity_percentage_computes_to_alpha_fraction() {
  let dom = dom::parse_html(r#"<div id="target"></div>"#).expect("parse html");
  let stylesheet = parse_stylesheet("#target { opacity: 50%; }").expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let target = find_by_id(&styled, "target").expect("target element");
  assert!(
    (target.styles.opacity - 0.5).abs() < 1e-6,
    "expected opacity 0.5, got {}",
    target.styles.opacity
  );
}

#[test]
fn opacity_percentage_is_clamped() {
  let dom = dom::parse_html(r#"<div id="target"></div>"#).expect("parse html");
  let stylesheet = parse_stylesheet("#target { opacity: 150%; }").expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let target = find_by_id(&styled, "target").expect("target element");
  assert!(
    (target.styles.opacity - 1.0).abs() < 1e-6,
    "expected opacity 1.0, got {}",
    target.styles.opacity
  );

  let stylesheet = parse_stylesheet("#target { opacity: -10%; }").expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let target = find_by_id(&styled, "target").expect("target element");
  assert!(
    (target.styles.opacity - 0.0).abs() < 1e-6,
    "expected opacity 0.0, got {}",
    target.styles.opacity
  );
}
