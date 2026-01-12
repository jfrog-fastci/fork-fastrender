#![cfg(test)]

use super::{Document, MutationObserverId, MutationObserverInit, MutationRecordType};
use selectors::context::QuirksMode;
use std::collections::HashMap;

#[test]
fn remap_node_ids_updates_queued_records_and_observed_targets() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let target = doc.create_element("div", "");
  doc.append_child(root, target).unwrap();

  let prev = doc.create_element("span", "");
  let removed = doc.create_element("i", "");
  let next = doc.create_element("span", "");
  doc.append_child(target, prev).unwrap();
  doc.append_child(target, removed).unwrap();
  doc.append_child(target, next).unwrap();

  let added = doc.create_element("b", "");

  let observer: MutationObserverId = 1;
  doc
    .mutation_observer_observe(
      observer,
      target,
      MutationObserverInit {
        child_list: true,
        ..Default::default()
      },
    )
    .unwrap();

  // Queue a childList record that references:
  // - target
  // - added_nodes / removed_nodes
  // - previous_sibling / next_sibling
  doc.replace_child(target, added, removed).unwrap();

  let target_new = doc.create_element("div", "");
  let prev_new = doc.create_element("span", "");
  let added_new = doc.create_element("b", "");
  let removed_new = doc.create_element("i", "");
  let next_new = doc.create_element("span", "");

  // Simulate adoption: registrations move with the node, but observer state + queued records still
  // reference the old NodeIds until we remap.
  doc.mutation_observer_move_registrations(target, target_new);

  let mapping: HashMap<_, _> = HashMap::from([
    (target, target_new),
    (prev, prev_new),
    (added, added_new),
    (removed, removed_new),
    (next, next_new),
  ]);
  doc.mutation_observer_remap_node_ids(&mapping);

  // Remapping should not interfere with microtask scheduling.
  assert!(doc.take_mutation_observer_microtask_needed());

  let records = doc.mutation_observer_take_records(observer);
  assert_eq!(records.len(), 1);
  let record = &records[0];
  assert_eq!(record.type_, MutationRecordType::ChildList);
  assert_eq!(record.target, target_new);
  assert_eq!(record.added_nodes, vec![added_new]);
  assert_eq!(record.removed_nodes, vec![removed_new]);
  assert_eq!(record.previous_sibling, Some(prev_new));
  assert_eq!(record.next_sibling, Some(next_new));

  // Ensure the observer node list was remapped: disconnect should remove the registration from
  // `target_new` (the moved registrations), not `target`.
  doc.mutation_observer_disconnect(observer);
  let child = doc.create_element("x", "");
  doc.append_child(target_new, child).unwrap();
  assert!(
    doc.mutation_observer_take_records(observer).is_empty(),
    "disconnect should remove registrations after remap"
  );
}

#[test]
fn remap_node_ids_leaves_unmapped_entries_unchanged() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let target = doc.create_element("div", "");
  doc.append_child(root, target).unwrap();

  let prev = doc.create_element("span", "");
  let removed = doc.create_element("i", "");
  let next = doc.create_element("span", "");
  doc.append_child(target, prev).unwrap();
  doc.append_child(target, removed).unwrap();
  doc.append_child(target, next).unwrap();

  let added = doc.create_element("b", "");

  let observer: MutationObserverId = 2;
  doc
    .mutation_observer_observe(
      observer,
      target,
      MutationObserverInit {
        child_list: true,
        ..Default::default()
      },
    )
    .unwrap();
  doc.replace_child(target, added, removed).unwrap();

  let target_new = doc.create_element("div", "");
  let added_new = doc.create_element("b", "");

  doc.mutation_observer_move_registrations(target, target_new);

  // Only remap some entries; others should remain as-is.
  let mapping: HashMap<_, _> = HashMap::from([(target, target_new), (added, added_new)]);
  doc.mutation_observer_remap_node_ids(&mapping);

  let records = doc.mutation_observer_take_records(observer);
  assert_eq!(records.len(), 1);
  let record = &records[0];
  assert_eq!(record.target, target_new);
  assert_eq!(record.added_nodes, vec![added_new]);
  assert_eq!(record.removed_nodes, vec![removed]);
  assert_eq!(record.previous_sibling, Some(prev));
  assert_eq!(record.next_sibling, Some(next));
}
