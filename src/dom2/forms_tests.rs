#![cfg(test)]

use selectors::context::QuirksMode;

use super::{Document, NodeId, NodeKind};

fn find_first_text_child(doc: &Document, parent: NodeId) -> Option<NodeId> {
  doc
    .node(parent)
    .children
    .iter()
    .copied()
    .find(|&child| doc.node(child).parent == Some(parent) && matches!(doc.node(child).kind, NodeKind::Text { .. }))
}

#[test]
fn input_value_and_checked_use_internal_state_with_dirty_flags() {
  let html = "<!doctype html><html><body><input id=i type=checkbox value=foo checked></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("i").expect("input element");

  assert_eq!(doc.get_attribute(input, "value").unwrap(), Some("foo"));
  assert!(doc.has_attribute(input, "checked").unwrap());
  assert_eq!(doc.input_value(input).unwrap(), "foo");
  assert!(doc.input_checked(input).unwrap());

  // IDL property setters must not mutate attributes.
  doc.set_input_value(input, "bar").unwrap();
  doc.set_input_checked(input, false).unwrap();
  assert_eq!(doc.get_attribute(input, "value").unwrap(), Some("foo"));
  assert!(doc.has_attribute(input, "checked").unwrap());

  // Dirty value/checkedness must ignore subsequent attribute changes.
  doc.set_attribute(input, "value", "baz").unwrap();
  doc.remove_attribute(input, "checked").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "bar");
  assert!(!doc.input_checked(input).unwrap());

  // Reset restores from attributes and clears dirty flags.
  doc.reset_input(input).unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "baz");
  assert!(!doc.input_checked(input).unwrap());

  // When not dirty, attribute changes re-sync internal state.
  doc.set_attribute(input, "value", "qux").unwrap();
  doc.set_bool_attribute(input, "checked", true).unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "qux");
  assert!(doc.input_checked(input).unwrap());
}

#[test]
fn textarea_value_uses_text_content_until_dirty() {
  let html = "<!doctype html><html><body><textarea id=t>hello</textarea></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let textarea = doc.get_element_by_id("t").expect("textarea element");
  let text = find_first_text_child(&doc, textarea).expect("textarea text node");

  assert_eq!(doc.textarea_value(textarea).unwrap(), "hello");

  // While not dirty, changes to descendant text nodes are observable via `.value`.
  doc.set_text_data(text, "world").unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "world");

  doc.set_textarea_value(textarea, "dirty").unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "dirty");
  // `.value` does not mutate the underlying text nodes.
  assert_eq!(doc.text_data(text).unwrap(), "world");

  // Once dirty, descendant text changes no longer affect `.value`.
  doc.set_text_data(text, "ignored").unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "dirty");

  // Reset returns to derived value semantics.
  doc.reset_textarea(textarea).unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "ignored");
}

#[test]
fn option_selectedness_uses_internal_state_with_dirty_flag() {
  let html =
    "<!doctype html><html><body><select><option id=o selected>One</option></select></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let option = doc.get_element_by_id("o").expect("option element");

  assert!(doc.has_attribute(option, "selected").unwrap());
  assert!(doc.option_selected(option).unwrap());

  // IDL property setter must not mutate attributes.
  doc.set_option_selected(option, false).unwrap();
  assert!(doc.has_attribute(option, "selected").unwrap());
  assert!(!doc.option_selected(option).unwrap());

  // While dirty, attribute changes must not affect selectedness.
  doc.remove_attribute(option, "selected").unwrap();
  doc.set_bool_attribute(option, "selected", true).unwrap();
  assert!(doc.has_attribute(option, "selected").unwrap());
  assert!(!doc.option_selected(option).unwrap());

  // Reset restores from attributes and clears dirty flags.
  doc.reset_option(option).unwrap();
  assert!(doc.option_selected(option).unwrap());

  // When not dirty, attribute changes re-sync internal state.
  doc.remove_attribute(option, "selected").unwrap();
  assert!(!doc.option_selected(option).unwrap());
  doc.set_bool_attribute(option, "selected", true).unwrap();
  assert!(doc.option_selected(option).unwrap());
}

#[test]
fn state_is_initialized_for_dom_created_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let input = doc.create_element("input", "");
  let textarea = doc.create_element("textarea", "");
  let option = doc.create_element("option", "");

  assert_eq!(doc.input_value(input).unwrap(), "");
  assert!(!doc.input_checked(input).unwrap());

  assert_eq!(doc.textarea_value(textarea).unwrap(), "");

  assert!(!doc.option_selected(option).unwrap());
}
