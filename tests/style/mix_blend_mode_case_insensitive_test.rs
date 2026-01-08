use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::MixBlendMode;

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
fn mix_blend_mode_keyword_is_ascii_case_insensitive() {
  let dom = dom::parse_html(r#"<div id="target"></div>"#).expect("parse html");
  let stylesheet = parse_stylesheet("#target { mix-blend-mode: DIFFERENCE; }").expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.mix_blend_mode, MixBlendMode::Difference);
}

