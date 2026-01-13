#![cfg(test)]

use selectors::context::QuirksMode;

use crate::dom::DomNode;

use super::{Document, NodeId, NodeKind};

fn find_first_text_child(doc: &Document, parent: NodeId) -> Option<NodeId> {
  doc.node(parent).children.iter().copied().find(|&child| {
    doc.node(child).parent == Some(parent) && matches!(doc.node(child).kind, NodeKind::Text { .. })
  })
}

fn find_dom_by_id<'a>(root: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_shadow_root_for_host(doc: &Document, host: NodeId) -> Option<NodeId> {
  let host_node = doc.nodes().get(host.index())?;
  host_node.children.iter().copied().find(|&child| {
    let Some(child_node) = doc.nodes().get(child.index()) else {
      return false;
    };
    child_node.parent == Some(host) && matches!(child_node.kind, NodeKind::ShadowRoot { .. })
  })
}

#[test]
fn input_value_and_checked_use_internal_state_with_dirty_flags() {
  let html =
    "<!doctype html><html><body><input id=i type=checkbox value=foo checked></body></html>";
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
fn checkbox_value_defaults_to_on_when_value_attribute_missing() {
  let html = "<!doctype html><html><body><input id=i type=checkbox checked></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("i").expect("input element");
  assert_eq!(doc.get_attribute(input, "value").unwrap(), None);
  assert_eq!(doc.input_value(input).unwrap(), "on");

  // Dirty value can override to the empty string without adding a `value` attribute.
  doc.set_input_value(input, "").unwrap();
  assert_eq!(doc.get_attribute(input, "value").unwrap(), None);
  assert_eq!(doc.input_value(input).unwrap(), "");

  // Reset restores the default value derived from the content attribute state.
  doc.reset_input(input).unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "on");
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

#[test]
fn form_control_property_setters_record_form_state_mutations() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let input = doc.create_element("input", "");
  let textarea = doc.create_element("textarea", "");

  doc.set_input_value(input, "hello").unwrap();
  let mutations = doc.take_mutations();
  assert!(mutations.form_state_changed.contains(&input));
  assert!(mutations.attribute_changed.is_empty());
  assert!(mutations.text_changed.is_empty());
  assert!(mutations.child_list_changed.is_empty());

  doc.set_input_checked(input, true).unwrap();
  let mutations = doc.take_mutations();
  assert!(mutations.form_state_changed.contains(&input));
  assert!(mutations.attribute_changed.is_empty());
  assert!(mutations.text_changed.is_empty());
  assert!(mutations.child_list_changed.is_empty());

  doc.set_textarea_value(textarea, "world").unwrap();
  let mutations = doc.take_mutations();
  assert!(mutations.form_state_changed.contains(&textarea));
  assert!(mutations.attribute_changed.is_empty());
  assert!(mutations.text_changed.is_empty());
  assert!(mutations.child_list_changed.is_empty());
}

#[test]
fn radio_group_exclusivity_same_name() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<input id=a type=radio name=g checked>",
    "<input id=b type=radio name=g>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let a = doc.get_element_by_id("a").expect("radio a");
  let b = doc.get_element_by_id("b").expect("radio b");

  assert!(doc.input_checked(a).unwrap());
  assert!(!doc.input_checked(b).unwrap());

  doc.set_input_checked(b, true).unwrap();
  assert!(
    !doc.input_checked(a).unwrap(),
    "other radio must be unchecked"
  );
  assert!(doc.input_checked(b).unwrap());

  // IDL property setters must not mutate attributes.
  assert!(doc.has_attribute(a, "checked").unwrap());

  // Unchecked radios become dirty when unchecked due to another radio being checked.
  assert!(
    doc
      .input_states
      .get(a.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| s.dirty_checkedness),
    "unchecked radio should be dirty"
  );
  assert!(
    doc
      .input_states
      .get(b.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| s.dirty_checkedness),
    "checked radio should be dirty after IDL setter"
  );
}

#[test]
fn radio_group_exclusivity_different_names_no_cross_effects() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<input id=a type=radio name=g checked>",
    "<input id=b type=radio name=h>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let a = doc.get_element_by_id("a").expect("radio a");
  let b = doc.get_element_by_id("b").expect("radio b");

  doc.set_input_checked(b, true).unwrap();
  assert!(
    doc.input_checked(a).unwrap(),
    "different name must not be unchecked"
  );
  assert!(doc.input_checked(b).unwrap());

  assert!(
    doc
      .input_states
      .get(a.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| !s.dirty_checkedness),
    "unaffected radios should not become dirty"
  );
}

#[test]
fn radio_group_exclusivity_different_tree_roots_no_cross_effects() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<input id=inner type=radio name=g>",
    "</template>",
    "</div>",
    "<input id=outer type=radio name=g checked>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("shadow host");
  let shadow_root = find_shadow_root_for_host(&doc, host).expect("shadow root");
  let inner = doc
    .get_element_by_id_from(shadow_root, "inner")
    .expect("inner radio");
  let outer = doc.get_element_by_id("outer").expect("outer radio");

  assert!(!doc.input_checked(inner).unwrap());
  assert!(doc.input_checked(outer).unwrap());

  doc.set_input_checked(inner, true).unwrap();
  assert!(doc.input_checked(inner).unwrap());
  assert!(
    doc.input_checked(outer).unwrap(),
    "shadow tree root must not affect light DOM radio group"
  );
}

#[test]
fn radio_group_exclusivity_form_reset_restores_last_default_checked() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<form id=f>",
    "<input id=r1 type=radio name=g checked>",
    "<input id=r2 type=radio name=g checked>",
    "</form>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let form = doc.get_element_by_id("f").expect("form");
  let r1 = doc.get_element_by_id("r1").expect("r1");
  let r2 = doc.get_element_by_id("r2").expect("r2");

  // User/runtime selection makes r1 the checked one.
  doc.set_input_checked(r1, true).unwrap();
  assert!(doc.input_checked(r1).unwrap());
  assert!(!doc.input_checked(r2).unwrap());

  // Reset returns to defaults. When multiple `checked` attributes exist, only the last-in-tree-order
  // radio should end up checked (legacy behavior).
  doc.form_reset(form).unwrap();
  assert!(!doc.input_checked(r1).unwrap());
  assert!(doc.input_checked(r2).unwrap());

  // Reset clears dirty flags.
  assert!(
    doc
      .input_states
      .get(r1.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| !s.dirty_checkedness),
    "reset should clear dirty_checkedness"
  );
  assert!(
    doc
      .input_states
      .get(r2.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| !s.dirty_checkedness),
    "reset should clear dirty_checkedness"
  );
}

#[test]
fn renderer_dom_snapshot_projects_runtime_form_control_state() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<input id=t value=foo>",
    "<input id=c type=checkbox>",
    "<input id=f type=file>",
    "<textarea id=ta>hello</textarea>",
    "<select><option id=o>One</option></select>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let text_input = doc.get_element_by_id("t").expect("text input");
  let checkbox = doc.get_element_by_id("c").expect("checkbox");
  let file_input = doc.get_element_by_id("f").expect("file input");
  let textarea = doc.get_element_by_id("ta").expect("textarea");
  let option = doc.get_element_by_id("o").expect("option");

  // Mutate runtime state without mutating content attributes.
  doc.set_input_value(text_input, "bar").unwrap();
  doc.set_input_checked(checkbox, true).unwrap();
  doc
    .set_input_value(file_input, "C:\\secret\\path.txt")
    .unwrap();
  doc.set_textarea_value(textarea, "world").unwrap();
  doc.set_option_selected(option, true).unwrap();

  let mut snapshot = doc.to_renderer_dom_with_mapping();
  doc.project_form_control_state_into_renderer_dom_snapshot(&mut snapshot.dom, &snapshot.mapping);

  let text_node = find_dom_by_id(&snapshot.dom, "t").expect("text input snapshot");
  assert_eq!(text_node.get_attribute_ref("value"), Some("bar"));

  let checkbox_node = find_dom_by_id(&snapshot.dom, "c").expect("checkbox snapshot");
  assert_eq!(
    checkbox_node.get_attribute_ref("value"),
    None,
    "default checkbox value should not synthesize a `value` attribute when missing"
  );
  assert!(
    checkbox_node.get_attribute_ref("checked").is_some(),
    "checkedness should be projected into `checked` attribute"
  );

  let file_node = find_dom_by_id(&snapshot.dom, "f").expect("file input snapshot");
  assert_eq!(
    file_node.get_attribute_ref("value"),
    None,
    "file input value must not be projected into markup attributes"
  );

  let textarea_node = find_dom_by_id(&snapshot.dom, "ta").expect("textarea snapshot");
  assert_eq!(
    textarea_node.get_attribute_ref("data-fastr-value"),
    Some("world"),
    "textarea runtime value should be projected into `data-fastr-value`"
  );

  let option_node = find_dom_by_id(&snapshot.dom, "o").expect("option snapshot");
  assert!(
    option_node.get_attribute_ref("selected").is_some(),
    "option selectedness should be projected into `selected` attribute"
  );

  // Reset textarea should remove dirty override attribute.
  doc.reset_textarea(textarea).unwrap();
  let mut snapshot = doc.to_renderer_dom_with_mapping();
  doc.project_form_control_state_into_renderer_dom_snapshot(&mut snapshot.dom, &snapshot.mapping);
  let textarea_node = find_dom_by_id(&snapshot.dom, "ta").expect("textarea snapshot");
  assert_eq!(
    textarea_node.get_attribute_ref("data-fastr-value"),
    None,
    "non-dirty textarea should not carry `data-fastr-value` override"
  );
}

#[test]
fn renderer_snapshot_reflects_form_control_internal_state_and_mapping_remains_aligned() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<input id=i checked>",
    "<input id=c type=checkbox>",
    "<textarea id=t>hello</textarea>",
    "<select multiple><option id=o selected>One</option></select>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("i").expect("input element");
  let checkbox = doc.get_element_by_id("c").expect("checkbox element");
  let textarea = doc.get_element_by_id("t").expect("textarea element");
  let option = doc.get_element_by_id("o").expect("option element");

  // Mutate internal (IDL) state without touching content attributes.
  doc.set_input_value(input, "bar").unwrap();
  doc.set_input_checked(input, true).unwrap();
  doc.set_input_checked(checkbox, true).unwrap();
  doc.set_textarea_value(textarea, "dirty").unwrap();
  doc.set_option_selected(option, false).unwrap();

  assert_eq!(
    doc.get_attribute(input, "value").unwrap(),
    None,
    "set_input_value must not mutate content attributes"
  );
  assert!(
    doc.has_attribute(input, "checked").unwrap(),
    "set_input_checked must not mutate content attributes"
  );
  assert!(
    !doc.has_attribute(checkbox, "checked").unwrap(),
    "set_input_checked must not mutate content attributes"
  );
  assert!(
    doc.has_attribute(option, "selected").unwrap(),
    "set_option_selected must not mutate content attributes"
  );

  let mut snapshot = doc.to_renderer_dom_with_mapping();

  // Helper: round-trip a dom2 NodeId to a renderer preorder id and back, then find the snapshot
  // node and assert we got the expected element.
  let mut get_snapshot_node = |node_id: NodeId, expected_id: &str| {
    let preorder = snapshot
      .mapping
      .preorder_for_node_id(node_id)
      .expect("missing preorder id for connected node");
    assert_eq!(
      snapshot.mapping.node_id_for_preorder(preorder),
      Some(node_id),
      "reverse renderer mapping mismatch"
    );

    let node = crate::dom::find_node_mut_by_preorder_id(&mut snapshot.dom, preorder)
      .expect("missing renderer node for preorder id");
    assert_eq!(node.get_attribute_ref("id"), Some(expected_id));
    node
  };

  // <input>: current value must be reflected into the snapshot.
  {
    let input_node = get_snapshot_node(input, "i");
    assert_eq!(input_node.get_attribute_ref("value"), Some("bar"));
    assert_eq!(
      input_node.get_attribute_ref("checked"),
      None,
      "expected `checked` to be absent for non-checkable inputs"
    );
  }

  // <input type=checkbox>: checkedness must be reflected into the snapshot.
  {
    let checkbox_node = get_snapshot_node(checkbox, "c");
    assert!(
      checkbox_node.get_attribute_ref("checked").is_some(),
      "expected checked attribute on snapshot checkbox"
    );
  }

  // <textarea>: current value must be reflected via `data-fastr-value`.
  {
    let textarea_node = get_snapshot_node(textarea, "t");
    assert_eq!(
      textarea_node.get_attribute_ref("data-fastr-value"),
      Some("dirty")
    );
  }

  // <option>: selectedness must be reflected into the snapshot.
  {
    let option_node = get_snapshot_node(option, "o");
    assert!(
      option_node.get_attribute_ref("selected").is_none(),
      "expected no `selected` attribute on snapshot option when selectedness=false"
    );
  }
}

#[test]
fn renderer_snapshot_overrides_content_attributes_with_internal_form_state() {
  let html = concat!(
    "<!doctype html><html><body>",
    "<input id=t value=foo>",
    "<input id=c type=checkbox checked>",
    "<textarea id=ta>hello</textarea>",
    "<select><option id=o selected>One</option></select>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let text_input = doc.get_element_by_id("t").expect("text input");
  let checkbox = doc.get_element_by_id("c").expect("checkbox");
  let textarea = doc.get_element_by_id("ta").expect("textarea");
  let option = doc.get_element_by_id("o").expect("option");

  // Mutate internal state without mutating content attributes.
  doc.set_input_value(text_input, "bar").unwrap();
  doc.set_input_checked(checkbox, false).unwrap();
  doc.set_textarea_value(textarea, "world").unwrap();
  doc.set_option_selected(option, false).unwrap();

  assert_eq!(doc.get_attribute(text_input, "value").unwrap(), Some("foo"));
  assert!(
    doc.has_attribute(checkbox, "checked").unwrap(),
    "set_input_checked must not mutate content attributes"
  );
  assert!(
    doc.has_attribute(option, "selected").unwrap(),
    "set_option_selected must not mutate content attributes"
  );

  let snapshot = doc.to_renderer_dom();

  let text_node = find_dom_by_id(&snapshot, "t").expect("text input snapshot");
  assert_eq!(text_node.get_attribute_ref("value"), Some("bar"));

  let checkbox_node = find_dom_by_id(&snapshot, "c").expect("checkbox snapshot");
  assert_eq!(
    checkbox_node.get_attribute_ref("checked"),
    None,
    "snapshot checkbox should reflect checkedness=false"
  );

  let textarea_node = find_dom_by_id(&snapshot, "ta").expect("textarea snapshot");
  assert_eq!(
    textarea_node.get_attribute_ref("data-fastr-value"),
    Some("world")
  );

  let option_node = find_dom_by_id(&snapshot, "o").expect("option snapshot");
  assert_eq!(
    option_node.get_attribute_ref("selected"),
    None,
    "snapshot option should reflect selectedness=false"
  );

  // Resetting the textarea should remove the runtime override attribute in the snapshot.
  doc.reset_textarea(textarea).unwrap();
  let snapshot = doc.to_renderer_dom();
  let textarea_node = find_dom_by_id(&snapshot, "ta").expect("textarea snapshot");
  assert_eq!(textarea_node.get_attribute_ref("data-fastr-value"), None);
}

#[test]
fn renderer_subtree_snapshot_reflects_form_control_internal_state() {
  let html = "<!doctype html><html><body><input id=i value=foo></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("i").expect("input element");

  doc.set_input_value(input, "bar").unwrap();
  assert_eq!(
    doc.get_attribute(input, "value").unwrap(),
    Some("foo"),
    "set_input_value must not mutate content attributes"
  );

  let subtree = doc
    .to_renderer_dom_subtree(input)
    .expect("subtree snapshot");
  assert_eq!(subtree.get_attribute_ref("id"), Some("i"));
  assert_eq!(
    subtree.get_attribute_ref("value"),
    Some("bar"),
    "subtree snapshot should reflect the current input value"
  );
}

#[test]
fn file_input_value_is_never_script_settable() {
  let html = "<!doctype html><html><body><input id=f type=file value=foo></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("f").expect("file input element");

  // Authored file input state is stripped at parse time.
  assert_eq!(doc.get_attribute(input, "value").unwrap(), None);
  assert_eq!(doc.input_value(input).unwrap(), "");

  // Mutating the `value` content attribute must not affect the live `.value` state for file inputs.
  doc.set_attribute(input, "value", "foo").unwrap();
  assert_eq!(doc.get_attribute(input, "value").unwrap(), Some("foo"));
  assert_eq!(doc.input_value(input).unwrap(), "");

  // Non-empty IDL assignments are ignored and must not bump mutation generation.
  let gen_before = doc.mutation_generation();
  doc.set_input_value(input, "bar").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");
  assert_eq!(doc.mutation_generation(), gen_before);

  // Empty IDL assignments are allowed (to clear an existing selection).
  doc.set_input_value(input, "").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");

  // Reset keeps file inputs empty regardless of any `value` content attribute.
  doc.reset_input(input).unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");

  // After reset, dirty flags must be cleared so leaving `type=file` resyncs from the `value`
  // content attribute when not dirty.
  doc.set_attribute(input, "value", "baz").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");
  doc.set_attribute(input, "type", "text").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "baz");

  // Changing type from text -> file clears the live value state when not dirty.
  doc.set_attribute(input, "type", "file").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");
}

#[test]
fn file_input_form_reset_clears_value_and_dirty_flags() {
  let html = "<!doctype html><html><body><form id=f><input id=i type=file></form></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let form = doc.get_element_by_id("f").expect("form element");
  let input = doc.get_element_by_id("i").expect("file input element");

  doc.set_input_value(input, "").unwrap();
  doc.set_attribute(input, "value", "foo").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");

  doc.form_reset(form).unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "");

  // Dirty flags must be cleared so type changes away from `file` can sync from the value attribute.
  doc.set_attribute(input, "type", "text").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "foo");
}
