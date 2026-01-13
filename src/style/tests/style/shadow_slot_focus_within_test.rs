use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::dom::enumerate_dom_ids;
use crate::dom::DomNode;
use crate::interaction::InteractionState;
use crate::style::cascade::StyledNode;
use crate::style::cascade::{apply_styles_with_interaction_state, apply_styles_with_target};
use crate::style::display::Display;

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
fn focus_within_considers_slotted_descendants() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          #wrap:focus-within { display: inline; }
         #wrap { display: block; }
        </style>
        <div id="wrap"><slot></slot></div>
      </template>
      <input id="slotted" type="text" />
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let slotted_id = dom_node_id_by_id(&dom, "slotted");
  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(slotted_id);
  interaction_state.focus_visible = false;
  interaction_state.set_focus_chain(vec![slotted_id]);
  let stylesheet = parse_stylesheet("").expect("stylesheet");
  let styled = apply_styles_with_interaction_state(&dom, &stylesheet, Some(&interaction_state));

  let wrap = find_by_id(&styled, "wrap").expect("wrap element");
  assert_eq!(wrap.styles.display, Display::Inline);
}

#[test]
fn target_within_considers_slotted_descendants() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          #wrap:target-within { display: inline; }
          #wrap { display: block; }
        </style>
        <div id="wrap"><slot></slot></div>
      </template>
      <input id="slotted" type="text" />
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet("").expect("stylesheet");
  let styled = apply_styles_with_target(&dom, &stylesheet, Some("#slotted"));

  let wrap = find_by_id(&styled, "wrap").expect("wrap element");
  assert_eq!(wrap.styles.display, Display::Inline);
}
