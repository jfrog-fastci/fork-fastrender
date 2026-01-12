#![cfg(test)]

use super::{Document, MutationObserverAgent, MutationObserverId, MutationObserverInit};
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn shared_agent_does_not_corrupt_other_document_observer_state_on_delivery() {
  let agent = Rc::new(RefCell::new(MutationObserverAgent::new()));
  let mut doc_a = Document::new_with_mutation_observer_agent(
    QuirksMode::NoQuirks,
    /* scripting_enabled */ true,
    Rc::clone(&agent),
  );
  let mut doc_b = Document::new_with_mutation_observer_agent(
    QuirksMode::NoQuirks,
    /* scripting_enabled */ true,
    Rc::clone(&agent),
  );

  let root_a = doc_a.root();
  let root_b = doc_b.root();

  // Ensure the observed nodes have the same `NodeId` indices in both documents to exercise the case
  // where a shared agent would otherwise conflate per-document node-index spaces.
  let target_a = doc_a.create_element("div", "");
  doc_a.append_child(root_a, target_a).unwrap();
  let target_b = doc_b.create_element("div", "");
  doc_b.append_child(root_b, target_b).unwrap();

  let observer_a: MutationObserverId = 1;
  let observer_b: MutationObserverId = 2;
  let options = MutationObserverInit {
    child_list: true,
    ..Default::default()
  };
  doc_a
    .mutation_observer_observe(observer_a, target_a, options.clone())
    .unwrap();
  doc_b
    .mutation_observer_observe(observer_b, target_b, options)
    .unwrap();

  // Queue one record in each document.
  let child_a = doc_a.create_element("span", "");
  doc_a.append_child(target_a, child_a).unwrap();
  let child_b = doc_b.create_element("span", "");
  doc_b.append_child(target_b, child_b).unwrap();

  // Microtask scheduling is tracked on the shared agent.
  assert!(doc_a.take_mutation_observer_microtask_needed());
  assert!(!doc_b.take_mutation_observer_microtask_needed());

  // Deliver records by draining the shared agent from one document.
  //
  // This should not corrupt the other document's observer state (e.g. by clearing its node list),
  // since that would prevent `disconnect()` from removing registrations from the right nodes.
  let deliveries = doc_a.mutation_observer_take_deliveries();
  assert_eq!(deliveries.len(), 2);
  assert!(deliveries
    .iter()
    .any(|(id, records)| *id == observer_a && records.len() == 1));
  assert!(deliveries
    .iter()
    .any(|(id, records)| *id == observer_b && records.len() == 1));

  // Disconnecting the observer in `doc_b` should remove its registrations from `doc_b`'s nodes even
  // though delivery was performed through `doc_a`.
  doc_b.mutation_observer_disconnect(observer_b);
  let child_b2 = doc_b.create_element("span", "");
  doc_b.append_child(target_b, child_b2).unwrap();
  assert!(
    doc_b.mutation_observer_take_records(observer_b).is_empty(),
    "disconnect must remove per-node registrations even when deliveries were drained from another document"
  );
}

