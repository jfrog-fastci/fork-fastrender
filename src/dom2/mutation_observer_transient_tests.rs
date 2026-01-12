#![cfg(test)]

use super::{Document, MutationObserverInit, MutationRecordType, NodeKind};
use selectors::context::QuirksMode;

#[test]
fn mutation_observer_subtree_attributes_survives_remove_until_delivery() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, child).unwrap();

  doc
    .mutation_observer_observe(
      1,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  // Remove the subtree root.
  doc.remove_child(parent, child).unwrap();

  // Mutate the removed node before delivering mutation records. This should still be observed via a
  // transient registered observer installed at removal time.
  doc.set_attribute(child, "data-x", "1").unwrap();

  let deliveries = doc.mutation_observer_take_deliveries();
  let records = deliveries
    .into_iter()
    .find(|(id, _)| *id == 1)
    .map(|(_, recs)| recs)
    .unwrap_or_default();

  assert_eq!(records.len(), 1);
  assert_eq!(records[0].type_, MutationRecordType::Attributes);
  assert_eq!(records[0].target, child);
  assert_eq!(records[0].attribute_name.as_deref(), Some("data-x"));

  // After delivery, transient registrations should be cleaned up.
  doc.set_attribute(child, "data-y", "2").unwrap();
  assert!(doc.mutation_observer_take_deliveries().is_empty());
}

#[test]
fn mutation_observer_subtree_attributes_survives_move_until_delivery() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let container = doc.create_element("div", "");
  let old_parent = doc.create_element("div", "");
  let new_parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");

  doc.append_child(root, container).unwrap();
  doc.append_child(container, old_parent).unwrap();
  doc.append_child(container, new_parent).unwrap();
  doc.append_child(old_parent, child).unwrap();

  doc
    .mutation_observer_observe(
      1,
      old_parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  // Move `child` out of `old_parent`'s subtree.
  doc.append_child(new_parent, child).unwrap();
  assert!(doc.parent(child).unwrap() == Some(new_parent));

  // Mutate after the move but before delivery; should still be observed via a transient registered
  // observer created during the removal step of the move.
  doc.set_attribute(child, "data-x", "1").unwrap();

  let deliveries = doc.mutation_observer_take_deliveries();
  let records = deliveries
    .into_iter()
    .find(|(id, _)| *id == 1)
    .map(|(_, recs)| recs)
    .unwrap_or_default();

  assert_eq!(records.len(), 1);
  assert_eq!(records[0].type_, MutationRecordType::Attributes);
  assert_eq!(records[0].target, child);

  // After delivery, the old subtree observer should no longer apply.
  doc.set_attribute(child, "data-y", "2").unwrap();
  assert!(doc.mutation_observer_take_deliveries().is_empty());

  // Sanity check: we didn't accidentally observe `new_parent` (the moved node should still be
  // connected and structurally valid).
  assert!(matches!(&doc.node(new_parent).kind, NodeKind::Element { .. }));
}

#[test]
fn mutation_observer_observe_update_clears_transients() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, child).unwrap();

  doc
    .mutation_observer_observe(
      1,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  // Removing the node creates transient registrations so subtree observers can still see mutations
  // on the removed subtree until the next notification.
  doc.remove_child(parent, child).unwrap();

  // Updating the registration on the observed node should remove any transients sourced from that
  // registration (DOM observe() step).
  doc
    .mutation_observer_observe(
      1,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: false,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  doc.set_attribute(child, "data-x", "1").unwrap();
  assert!(doc.mutation_observer_take_deliveries().is_empty());
}
