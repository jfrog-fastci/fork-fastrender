use fastrender::dom::parse_html;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation::{
  activate_radio, append_text_to_input, append_text_to_textarea, backspace_input, backspace_textarea,
  remove_attr, set_attr, toggle_checkbox,
};

fn textarea_text(node: &fastrender::dom::DomNode) -> String {
  let mut out = String::new();
  for child in &node.children {
    if let Some(text) = child.text_content() {
      out.push_str(text);
    }
  }
  out
}

#[test]
fn attr_set_remove_is_case_insensitive_for_html() {
  let mut dom = parse_html(r#"<!doctype html><div id="x"></div>"#).unwrap();
  let mut index = DomIndex::build(&mut dom);
  let div_id = *index.id_by_element_id.get("x").unwrap();

  index
    .with_node_mut(div_id, |node| {
      assert_eq!(node.get_attribute_ref("id"), Some("x"));

      assert!(set_attr(node, "ID", "y"));
      assert_eq!(node.get_attribute_ref("id"), Some("y"));
      let id_count = node
        .attributes_iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("id"))
        .count();
      assert_eq!(id_count, 1);

      assert!(!set_attr(node, "id", "y"));

      assert!(remove_attr(node, "ID"));
      assert!(node.get_attribute_ref("id").is_none());
      assert!(!remove_attr(node, "id"));
    })
    .unwrap();
}

#[test]
fn checkbox_toggle_clears_indeterminate() {
  let mut dom = parse_html(
    r#"<!doctype html><input id="c" type="checkbox" checked indeterminate aria-checked="mixed">"#,
  )
  .unwrap();
  let mut index = DomIndex::build(&mut dom);
  let checkbox_id = *index.id_by_element_id.get("c").unwrap();

  index
    .with_node_mut(checkbox_id, |node| {
      assert!(node.get_attribute_ref("checked").is_some());
      assert!(node.get_attribute_ref("indeterminate").is_some());
      assert_eq!(node.get_attribute_ref("aria-checked"), Some("mixed"));

      assert!(toggle_checkbox(node));

      assert!(node.get_attribute_ref("checked").is_none());
      assert!(node.get_attribute_ref("indeterminate").is_none());
      assert!(node.get_attribute_ref("aria-checked").is_none());
    })
    .unwrap();
}

#[test]
fn radio_activation_unchecks_others_in_same_group() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <input id="r1" type="radio" name="g" checked>
      <input id="r2" type="radio" name="g">"#,
  )
  .unwrap();
  let index = DomIndex::build(&mut dom);
  let r2_id = *index.id_by_element_id.get("r2").unwrap();

  assert!(activate_radio(&mut dom, r2_id));

  let mut index = DomIndex::build(&mut dom);
  let r1_id = *index.id_by_element_id.get("r1").unwrap();
  let r2_id = *index.id_by_element_id.get("r2").unwrap();
  index
    .with_node_mut(r1_id, |node| assert!(node.get_attribute_ref("checked").is_none()))
    .unwrap();
  index
    .with_node_mut(r2_id, |node| assert!(node.get_attribute_ref("checked").is_some()))
    .unwrap();
}

#[test]
fn input_value_append_and_backspace() {
  let mut dom = parse_html(r#"<!doctype html><input id="i" value="ab">"#).unwrap();
  let mut index = DomIndex::build(&mut dom);
  let input_id = *index.id_by_element_id.get("i").unwrap();

  index
    .with_node_mut(input_id, |node| {
      assert!(append_text_to_input(node, "c"));
      assert_eq!(node.get_attribute_ref("value"), Some("abc"));

      assert!(backspace_input(node));
      assert_eq!(node.get_attribute_ref("value"), Some("ab"));
    })
    .unwrap();
}

#[test]
fn textarea_append_and_backspace_mutate_text_nodes() {
  let mut dom = parse_html(r#"<!doctype html><textarea id="t">ab</textarea>"#).unwrap();
  let mut index = DomIndex::build(&mut dom);
  let textarea_id = *index.id_by_element_id.get("t").unwrap();

  index
    .with_node_mut(textarea_id, |node| {
      assert_eq!(textarea_text(node), "ab");

      assert!(append_text_to_textarea(node, "c"));
      assert_eq!(textarea_text(node), "abc");

      assert!(backspace_textarea(node));
      assert_eq!(textarea_text(node), "ab");
    })
    .unwrap();
}
