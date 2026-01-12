#![cfg(test)]

use super::Document;
use selectors::context::QuirksMode;

#[test]
fn node_iterator_pre_remove_updates_reference_to_following_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  let d = doc.create_element("d", "");

  doc.append_child(container, a).unwrap();
  doc.append_child(a, b).unwrap();
  doc.append_child(container, d).unwrap();

  let iter = doc.create_node_iterator(container);
  doc.set_node_iterator_reference_and_pointer(iter, b, true);

  doc.remove_child(container, a).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(d));
  assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(true));
}

#[test]
fn node_iterator_pre_remove_updates_reference_to_parent_when_no_following_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(container, a).unwrap();
  doc.append_child(a, b).unwrap();

  let iter = doc.create_node_iterator(container);
  doc.set_node_iterator_reference_and_pointer(iter, b, true);

  doc.remove_child(container, a).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(container));
  assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(false));
}

#[test]
fn node_iterator_pre_remove_fragment_children_in_tree_order() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  let frag = doc.create_document_fragment();
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();

  let iter = doc.create_node_iterator(frag);
  doc.set_node_iterator_reference_and_pointer(iter, a, true);

  doc.append_child(container, frag).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(frag));
  assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(false));
}

#[test]
fn node_iterator_pre_remove_runs_when_moving_nodes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let parent1 = doc.create_element("div", "");
  let parent2 = doc.create_element("div", "");
  doc.append_child(root, parent1).unwrap();
  doc.append_child(root, parent2).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(parent1, a).unwrap();
  doc.append_child(parent1, b).unwrap();

  let iter = doc.create_node_iterator(parent1);
  doc.set_node_iterator_reference_and_pointer(iter, a, true);

  doc.append_child(parent2, a).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(b));
  assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(true));
}

#[test]
fn node_iterator_pre_remove_runs_for_inner_html() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(container, a).unwrap();
  doc.append_child(container, b).unwrap();

  let iter = doc.create_node_iterator(container);
  doc.set_node_iterator_reference_and_pointer(iter, b, true);

  doc.set_inner_html(container, "<span></span>").unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(container));
  assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(false));
}
