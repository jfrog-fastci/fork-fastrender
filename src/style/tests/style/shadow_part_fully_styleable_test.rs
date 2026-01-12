use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::dom::enumerate_dom_ids;
use crate::dom::DomNode;
use crate::interaction::InteractionState;
use crate::style::cascade::apply_styles;
use crate::style::cascade::apply_styles_with_interaction_state;
use crate::style::cascade::StyledNode;
use crate::style::content::ContentValue;
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
fn part_supports_state_pseudo_classes() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <button id="button" part="button">Button</button>
      </template>
    </x-host>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let button_id = dom_node_id_by_id(&dom, "button");
  let interaction_state = InteractionState {
    hover_chain: vec![button_id],
    ..InteractionState::default()
  };
  let stylesheet = parse_stylesheet("x-host::part(button):hover { color: rgb(1, 2, 3); }")
    .expect("parse stylesheet");
  let styled = apply_styles_with_interaction_state(&dom, &stylesheet, Some(&interaction_state));

  let button = find_by_id(&styled, "button").expect("part element");
  assert_eq!(button.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn part_supports_tree_abiding_pseudo_elements() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <button id="button" part="button">Button</button>
      </template>
    </x-host>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet =
    parse_stylesheet(r#"x-host::part(button)::before { content: "x"; color: rgb(4, 5, 6); }"#)
      .expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let button = find_by_id(&styled, "button").expect("part element");
  let before = button.before_styles.as_ref().expect("generated ::before");
  assert_eq!(before.content_value, ContentValue::from_string("x"));
  assert_eq!(before.color, Rgba::rgb(4, 5, 6));
}
