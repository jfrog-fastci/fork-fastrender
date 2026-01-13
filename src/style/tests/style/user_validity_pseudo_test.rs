use crate::css::parser::parse_stylesheet;
use crate::dom::{self, enumerate_dom_ids, DomNode};
use crate::interaction::InteractionState;
use crate::style::cascade::apply_styles_with_media_target_and_interaction_state;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;

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

fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id_attr) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("no element with id attribute {id_attr:?}");
}

#[test]
fn user_invalid_does_not_match_without_user_validity() {
  let html = r#"
    <input id='r' required>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media_target_and_interaction_state(
    &dom,
    &stylesheet,
    &MediaContext::screen(800.0, 600.0),
    None,
    None,
  );

  assert_eq!(
    display(find_by_id(&styled, "r").expect("required input")),
    "inline",
    ":invalid should match but :user-invalid should not without user-validity"
  );
}

#[test]
fn user_invalid_matches_when_control_user_validity_set() {
  let html = r#"
    <input id='r' required>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let mut interaction_state = InteractionState::default();
  interaction_state
    .user_validity_mut()
    .insert(node_id_by_id_attr(&dom, "r"));
  let styled = apply_styles_with_media_target_and_interaction_state(
    &dom,
    &stylesheet,
    &MediaContext::screen(800.0, 600.0),
    None,
    Some(&interaction_state),
  );

  assert_eq!(
    display(find_by_id(&styled, "r").expect("required input")),
    "block"
  );
}

#[test]
fn user_invalid_matches_when_form_user_validity_set() {
  let html = r#"
    <form id='f'>
      <input id='in' required>
    </form>
    <input id='out' required>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let mut interaction_state = InteractionState::default();
  interaction_state
    .user_validity_mut()
    .insert(node_id_by_id_attr(&dom, "f"));
  let styled = apply_styles_with_media_target_and_interaction_state(
    &dom,
    &stylesheet,
    &MediaContext::screen(800.0, 600.0),
    None,
    Some(&interaction_state),
  );

  assert_eq!(
    display(find_by_id(&styled, "in").expect("input in form")),
    "block"
  );
  assert_eq!(
    display(find_by_id(&styled, "out").expect("input outside form")),
    "inline",
    "form hint should not apply to unrelated controls"
  );
}
