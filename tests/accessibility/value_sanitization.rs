use crate::common::accessibility::{find_by_id, find_json_node, render_accessibility_json};
use fastrender::api::FastRender;
use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::InteractionState;

fn find_dom_node_ptr_by_id(root: &DomNode, id: &str) -> Option<*const DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|v| v == id) {
      return Some(node as *const DomNode);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn accessibility_value_sanitizes_non_text_input_types_that_map_to_textbox() {
  let html = r#"
    <html><body>
      <input id="color" type="color" value="not-a-color">
      <input id="date" type="date" value="2024-99-99">
    </body></html>
  "#;

  let tree = render_accessibility_json(html);

  let color = find_json_node(&tree, "color").expect("color node");
  assert_eq!(color.get("role").and_then(|v| v.as_str()), Some("textbox"));
  assert_eq!(
    color.get("value").and_then(|v| v.as_str()),
    Some("#000000"),
    "expected invalid <input type=color> value to sanitize to #000000"
  );

  let date = find_json_node(&tree, "date").expect("date node");
  assert_eq!(date.get("role").and_then(|v| v.as_str()), Some("textbox"));
  let date_value = date.get("value").and_then(|v| v.as_str());
  assert!(
    date_value.is_none() || date_value == Some(""),
    "expected invalid <input type=date> value to sanitize to empty, got {date_value:?}"
  );
}

#[test]
fn accessibility_value_sanitizes_form_state_overrides_for_sanitized_input_types() {
  let html = r##"
    <html><body>
      <input id="color" type="color" value="#3366cc">
      <input id="date" type="date" value="2024-01-01">
    </body></html>
  "##;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");

  let ids = enumerate_dom_ids(&dom);
  let color_ptr = find_dom_node_ptr_by_id(&dom, "color").expect("color node");
  let date_ptr = find_dom_node_ptr_by_id(&dom, "date").expect("date node");
  let color_node_id = *ids.get(&color_ptr).expect("color node id");
  let date_node_id = *ids.get(&date_ptr).expect("date node id");

  let mut state = InteractionState::default();
  state
    .form_state_mut()
    .values
    .insert(color_node_id, "not-a-color".to_string());
  state
    .form_state_mut()
    .values
    .insert(date_node_id, "2024-99-99".to_string());

  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&state))
    .expect("accessibility tree");

  let color = find_by_id(&tree, "color").expect("color node");
  assert_eq!(color.role, "textbox");
  assert_eq!(
    color.value.as_deref(),
    Some("#000000"),
    "expected invalid color override to sanitize to #000000"
  );

  let date = find_by_id(&tree, "date").expect("date node");
  assert_eq!(date.role, "textbox");
  assert!(
    date.value.as_deref().is_none() || date.value.as_deref() == Some(""),
    "expected invalid date override to sanitize to empty, got {:?}",
    date.value
  );
}
