use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::dom::enumerate_dom_ids;
use crate::dom::DomNode;
use crate::interaction::InteractionState;
use crate::style::cascade::apply_styles_with_interaction_state;
use crate::style::cascade::StyledNode;
use crate::style::position::Position;
use crate::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node.is_element()
    && node
      .get_attribute_ref("id")
      .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_dom_by_id(child, id))
}

fn dom_node_id_by_id(dom: &DomNode, id: &str) -> usize {
  let ids = enumerate_dom_ids(dom);
  let node = find_dom_by_id(dom, id).expect("node");
  *ids
    .get(&(node as *const DomNode))
    .expect("expected DOM node id")
}

#[test]
fn fullscreen_pseudo_class_matches_active_fullscreen_element_and_applies_ua_defaults() {
  let html = r#"
    <video id="fs"></video>
    <video id="nonfs"></video>
  "#;
  let css = r#"
    video { color: rgb(0 0 0); }
    video:fullscreen { color: rgb(1 2 3); }
    video:not(:fullscreen) { color: rgb(4 5 6); }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let fs_id = dom_node_id_by_id(&dom, "fs");

  let mut interaction_state = InteractionState::default();
  interaction_state.set_fullscreen_element(Some(fs_id));

  let stylesheet = parse_stylesheet(css).expect("stylesheet");
  let styled = apply_styles_with_interaction_state(&dom, &stylesheet, Some(&interaction_state));

  let fs = find_by_id(&styled, "fs").expect("fs element");
  let nonfs = find_by_id(&styled, "nonfs").expect("nonfs element");

  // Author selectors must match the correct node.
  assert_eq!(fs.styles.color, Rgba::rgb(1, 2, 3));
  assert_eq!(nonfs.styles.color, Rgba::rgb(4, 5, 6));

  // UA `:fullscreen` defaults should apply only to the fullscreen element.
  assert_eq!(fs.styles.position, Position::Fixed);
  assert_eq!(fs.styles.z_index, Some(2147483647));
  assert_eq!(nonfs.styles.position, Position::Static);
  assert_ne!(nonfs.styles.z_index, Some(2147483647));
}

