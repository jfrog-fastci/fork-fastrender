#![cfg(test)]

use super::{Document, SlotAssignmentMode};
use crate::dom::ShadowRootMode;
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
  assert_eq!(
    doc.node_iterator_pointer_before_reference(iter),
    Some(false)
  );
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
  assert_eq!(
    doc.node_iterator_pointer_before_reference(iter),
    Some(false)
  );
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
  assert_eq!(
    doc.node_iterator_pointer_before_reference(iter),
    Some(false)
  );
}

#[test]
fn node_iterator_pre_remove_skips_shadow_root_when_finding_preceding_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  let host = doc.create_element("div", "");
  doc.append_child(container, host).unwrap();
  let shadow_root = doc
    .attach_shadow_root(
      host,
      ShadowRootMode::Open,
      /* clonable */ false,
      /* serializable */ false,
      /* delegates_focus */ false,
      SlotAssignmentMode::Named,
    )
    .unwrap();
  // Add a descendant inside the shadow root so raw traversal would "see" it.
  let _shadow_child = doc.create_element("span", "");
  doc.append_child(shadow_root, _shadow_child).unwrap();

  let after = doc.create_element("p", "");
  doc.append_child(container, after).unwrap();

  let iter = doc.create_node_iterator(container);
  doc.set_node_iterator_reference_and_pointer(iter, after, true);

  // Remove the last node so NodeIterator needs to fall back to the preceding node. The preceding
  // node should be the host element itself, not a descendant inside its shadow root.
  doc.remove_child(container, after).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(host));
  assert_eq!(
    doc.node_iterator_pointer_before_reference(iter),
    Some(false)
  );
}

#[test]
fn node_iterator_pre_remove_skips_inert_template_descendants_when_finding_preceding_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.root();
  let container = doc.create_element("div", "");
  doc.append_child(root, container).unwrap();

  // `create_element("template", ...)` sets `inert_subtree=true` for HTML templates.
  let template = doc.create_element("template", "");
  doc.append_child(container, template).unwrap();
  let _inert_child = doc.create_element("span", "");
  doc.append_child(template, _inert_child).unwrap();

  let after = doc.create_element("p", "");
  doc.append_child(container, after).unwrap();

  let iter = doc.create_node_iterator(container);
  doc.set_node_iterator_reference_and_pointer(iter, after, true);

  // Remove the last node so NodeIterator falls back to the preceding node. The preceding node
  // should be the `<template>` element itself, not its inert descendants.
  doc.remove_child(container, after).unwrap();

  assert_eq!(doc.node_iterator_reference(iter), Some(template));
  assert_eq!(
    doc.node_iterator_pointer_before_reference(iter),
    Some(false)
  );
}
