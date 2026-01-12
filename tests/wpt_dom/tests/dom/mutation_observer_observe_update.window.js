// META: script=/resources/testharness.js
//
// Curated MutationObserver coverage for FastRender's offline WPT DOM corpus.
// Focus: observe() update semantics and transient registered observer cleanup.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function mutation_observer_dummy_callback(_records, _observer) {}

function mutation_observer_observe_update_removes_transients_test() {
  clear_children(document.body);

  const parent = document.createElement("div");
  const child = document.createElement("div");
  const grandchild = document.createElement("div");
  child.appendChild(grandchild);
  parent.appendChild(child);
  document.body.appendChild(parent);

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(parent, { attributes: true, subtree: true });

  // Detach the subtree; subtree=true should install transient registrations so mutations in the
  // detached subtree are still observed until transient cleanup runs.
  parent.removeChild(child);

  grandchild.setAttribute("data-x", "before");
  const before_records = observer.takeRecords();
  assert_equals(before_records.length, 1, "transient registration should observe detached subtree");

  // Re-observing the same target must remove transient registrations sourced from the old
  // registration so detached subtrees stop being observed immediately.
  observer.observe(parent, { attributes: true, subtree: false });

  grandchild.setAttribute("data-x", "after");
  const after_records = observer.takeRecords();
  assert_equals(after_records.length, 0, "observe() update should clear transient registrations");

  observer.disconnect();
}

test(
  mutation_observer_observe_update_removes_transients_test,
  "MutationObserver.observe() update clears transient registrations sourced from prior registration"
);

function mutation_observer_observe_on_transient_target_applies_options_immediately_test() {
  clear_children(document.body);

  const parent = document.createElement("div");
  const child = document.createElement("div");
  const grandchild = document.createElement("div");
  child.appendChild(grandchild);
  parent.appendChild(child);
  document.body.appendChild(parent);

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(parent, { attributes: true, subtree: true });

  parent.removeChild(child);

  grandchild.setAttribute("data-x", "before");
  const before_records = observer.takeRecords();
  assert_equals(before_records.length, 1, "transient registration should observe detached subtree");

  // Observing the detached subtree root directly should replace transient options, so subtree=false
  // takes effect immediately.
  observer.observe(child, { attributes: true, subtree: false });

  grandchild.setAttribute("data-x", "after");
  const after_records = observer.takeRecords();
  assert_equals(after_records.length, 0, "observe(child) should not be shadowed by old transients");

  observer.disconnect();
}

test(
  mutation_observer_observe_on_transient_target_applies_options_immediately_test,
  "MutationObserver.observe() on a transient target applies new options immediately"
);

