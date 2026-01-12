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

#[test]
fn mutation_observer_observe_update_clears_transients_sourced_from_transients() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let observed_root = doc.create_element("div", "");
  let removed_parent = doc.create_element("div", "");
  let removed_child = doc.create_element("span", "");
  doc.append_child(root, observed_root).unwrap();
  doc.append_child(observed_root, removed_parent).unwrap();
  doc.append_child(removed_parent, removed_child).unwrap();

  // Observe attributes in the original connected subtree.
  doc
    .mutation_observer_observe(
      1,
      observed_root,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  // First removal installs a transient registered observer on `removed_parent`.
  doc.remove_child(observed_root, removed_parent).unwrap();
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_parent),
    1
  );
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_child),
    0
  );

  // Removing a descendant from within the detached subtree should create a transient registered
  // observer whose *source is the transient registered observer* on `removed_parent` (spec:
  // source is the `registered` entry from the inclusive ancestor's registered observer list).
  doc.remove_child(removed_parent, removed_child).unwrap();
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_child),
    1
  );

  // Updating the registration on the transient parent should remove transients sourced from that
  // transient registration (i.e. clear the nested transient on `removed_child`).
  doc
    .mutation_observer_observe(
      1,
      removed_parent,
      MutationObserverInit {
        attributes: true,
        subtree: false,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_child),
    0
  );

  // Mutations on `removed_child` are no longer observed.
  doc.set_attribute(removed_child, "data-x", "1").unwrap();
  assert!(doc.mutation_observer_take_deliveries().is_empty());
}

#[test]
fn mutation_observer_attribute_old_value_is_union_across_registrations() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, child).unwrap();

  // Seed an initial value so the attribute mutation has a non-null oldValue.
  doc.set_attribute(child, "data-x", "a").unwrap();

  // Register the same observer twice in the ancestor chain with different `attributeOldValue`
  // settings. The closer registration does *not* request oldValue, while the ancestor does.
  //
  // Per spec, the queued mutation record should still include oldValue if *any* matching
  // registration requests it.
  doc
    .mutation_observer_observe(
      1,
      child,
      MutationObserverInit {
        attributes: true,
        subtree: false,
        attribute_old_value: false,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();
  doc
    .mutation_observer_observe(
      1,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        attribute_old_value: true,
        ..MutationObserverInit::default()
      },
    )
    .unwrap();

  doc.set_attribute(child, "data-x", "b").unwrap();

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
  assert_eq!(records[0].old_value.as_deref(), Some("a"));
}
