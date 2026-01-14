use crate::css::parser::parse_stylesheet;
use crate::dom::{self, enumerate_dom_ids, DomNode};
use crate::interaction::InteractionState;
use crate::style::cascade::apply_styles_with_media_target_and_interaction_state;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::TopLayerKind;

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

fn find_styled_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_styled_by_id(child, id))
}

#[test]
fn fullscreen_element_is_promoted_to_top_layer() {
  let dom = dom::parse_html(r#"<div id="fs">fullscreen</div>"#).unwrap();
  let ids = enumerate_dom_ids(&dom);
  let fs = find_dom_by_id(&dom, "fs").expect("fullscreen element");
  let fs_id = *ids.get(&(fs as *const DomNode)).expect("node id");

  let mut interaction_state = InteractionState::default();
  interaction_state.set_fullscreen_element(Some(fs_id));

  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media_target_and_interaction_state(
    &dom,
    &stylesheet,
    &MediaContext::screen(800.0, 600.0),
    None,
    Some(&interaction_state),
  );

  let fs_styled = find_styled_by_id(&styled, "fs").expect("styled fullscreen element");
  assert_eq!(fs_styled.styles.top_layer, Some(TopLayerKind::Fullscreen));
}

#[test]
fn svg_fullscreen_element_is_not_promoted_to_top_layer() {
  let dom = dom::parse_html(
    r#"<svg>
      <g id="fs"></g>
    </svg>"#,
  )
  .unwrap();
  let ids = enumerate_dom_ids(&dom);
  let fs = find_dom_by_id(&dom, "fs").expect("fullscreen element");
  let fs_id = *ids.get(&(fs as *const DomNode)).expect("node id");

  let mut interaction_state = InteractionState::default();
  interaction_state.set_fullscreen_element(Some(fs_id));

  let stylesheet = parse_stylesheet("#fs { display: block; }").unwrap();
  let styled = apply_styles_with_media_target_and_interaction_state(
    &dom,
    &stylesheet,
    &MediaContext::screen(800.0, 600.0),
    None,
    Some(&interaction_state),
  );

  let fs_styled = find_styled_by_id(&styled, "fs").expect("styled fullscreen element");
  assert!(
    fs_styled.styles.top_layer.is_none(),
    "SVG :fullscreen must not participate in HTML top-layer semantics"
  );
}

