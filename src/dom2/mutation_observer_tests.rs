#![cfg(test)]

use super::{Document, MutationObserverId, MutationObserverInit, MutationRecordType};
use selectors::context::QuirksMode;

#[test]
fn transient_registered_observers_install_on_removed_root_only_and_track_descendants_until_delivery(
) {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let removed_root = doc.create_element("section", "");
  let descendant = doc.create_element("span", "");

  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, removed_root).unwrap();
  doc.append_child(removed_root, descendant).unwrap();

  let observer: MutationObserverId = 1;
  doc
    .mutation_observer_observe(
      observer,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..Default::default()
      },
    )
    .unwrap();

  // Removing `removed_root` from `parent` must install transient registered observers on
  // `removed_root` (the removed subtree root), not on every node in the removed subtree.
  doc.remove_child(parent, removed_root).unwrap();
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_root),
    1
  );
  assert_eq!(
    doc.mutation_observer_transient_registration_count(descendant),
    0
  );

  // Mutations to descendants after removal are still observed, because the transient registration on
  // `removed_root` has subtree=true and will match descendant mutations until delivery.
  doc.set_attribute(descendant, "id", "a").unwrap();
  assert!(
    doc.take_mutation_observer_microtask_needed(),
    "attribute mutation after removal should queue a mutation observer microtask"
  );

  let deliveries = doc.mutation_observer_take_deliveries();
  assert_eq!(deliveries.len(), 1);
  assert_eq!(deliveries[0].0, observer);
  let records = &deliveries[0].1;
  assert_eq!(records.len(), 1);
  assert_eq!(records[0].type_, MutationRecordType::Attributes);
  assert_eq!(records[0].target, descendant);
  assert_eq!(records[0].attribute_name.as_deref(), Some("id"));

  // After the microtask checkpoint, transient registered observers are removed (spec: notify
  // mutation observers).
  assert_eq!(
    doc.mutation_observer_transient_registration_count(removed_root),
    0
  );

  // Further mutations to the removed subtree are no longer observed through the old ancestor.
  doc.set_attribute(descendant, "id", "b").unwrap();
  assert!(!doc.take_mutation_observer_microtask_needed());
  assert!(doc.mutation_observer_take_deliveries().is_empty());
}

#[test]
fn transient_registered_observers_do_not_duplicate_on_repeated_remove_before_delivery() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");

  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, child).unwrap();

  let observer: MutationObserverId = 1;
  doc
    .mutation_observer_observe(
      observer,
      parent,
      MutationObserverInit {
        attributes: true,
        subtree: true,
        ..Default::default()
      },
    )
    .unwrap();

  // First removal: installs a transient registered observer on the removed subtree root (`child`).
  doc.remove_child(parent, child).unwrap();
  assert_eq!(doc.mutation_observer_transient_registration_count(child), 1);

  // Reinsert and remove again before delivering the microtask. The transient registration should
  // not be duplicated.
  doc.append_child(parent, child).unwrap();
  doc.remove_child(parent, child).unwrap();
  assert_eq!(doc.mutation_observer_transient_registration_count(child), 1);

  // Mutations to the removed subtree are still observed until delivery.
  doc.set_attribute(child, "data-x", "1").unwrap();
  let deliveries = doc.mutation_observer_take_deliveries();
  let records = deliveries
    .into_iter()
    .find(|(id, _)| *id == observer)
    .map(|(_, recs)| recs)
    .unwrap_or_default();
  assert_eq!(records.len(), 1);
  assert_eq!(records[0].type_, MutationRecordType::Attributes);
  assert_eq!(records[0].target, child);
  assert_eq!(records[0].attribute_name.as_deref(), Some("data-x"));

  // After delivery, transient registrations are removed.
  assert_eq!(doc.mutation_observer_transient_registration_count(child), 0);
}
