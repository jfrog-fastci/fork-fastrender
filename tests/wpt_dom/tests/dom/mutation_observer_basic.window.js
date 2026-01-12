// META: script=/resources/testharness.js
//
// Curated MutationObserver coverage for FastRender's offline WPT DOM corpus.
// Focus: basic construction, observe/takeRecords record contents, and disconnect semantics.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function mutation_observer_dummy_callback(_records, _observer) {}

function mutation_observer_exists_test() {
  assert_true(typeof MutationObserver === "function", "MutationObserver should exist");
}

test(mutation_observer_exists_test, "MutationObserver exists");

function mutation_observer_constructor_requires_new_test() {
  let threw = false;
  let name = "";
  try {
    // In browsers this is a TypeError: class constructors are not callable.
    MutationObserver();
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "Calling MutationObserver without new should throw");
  assert_equals(name, "TypeError", "MutationObserver() should throw TypeError");
}

test(
  mutation_observer_constructor_requires_new_test,
  "MutationObserver constructor requires new"
);

function mutation_observer_constructor_requires_callable_callback_test() {
  let threw = false;
  let name = "";
  try {
    new MutationObserver();
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "new MutationObserver() without a callback should throw");
  assert_equals(name, "TypeError", "MutationObserver requires a callable callback");
}

test(
  mutation_observer_constructor_requires_callable_callback_test,
  "MutationObserver constructor throws TypeError for invalid callback"
);

function mutation_observer_constructor_with_callable_callback_test() {
  let observer = null;
  try {
    observer = new MutationObserver(mutation_observer_dummy_callback);
  } catch (_e) {
    assert_unreached("new MutationObserver(callable) should not throw");
  }

  assert_true(observer !== null, "MutationObserver construction should return an object");
  assert_true(typeof observer.observe === "function", "observer.observe should exist");
  assert_true(typeof observer.disconnect === "function", "observer.disconnect should exist");
  assert_true(typeof observer.takeRecords === "function", "observer.takeRecords should exist");
}

test(
  mutation_observer_constructor_with_callable_callback_test,
  "MutationObserver can be constructed with a callable callback"
);

function mutation_observer_child_list_records_test() {
  clear_children(document.body);

  const container = document.createElement("div");
  document.body.appendChild(container);

  const a = document.createElement("span");
  container.appendChild(a);

  const b = document.createElement("span");

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(container, { childList: true });

  container.appendChild(b);
  container.removeChild(b);

  const records = observer.takeRecords();
  assert_equals(records.length, 2, "appendChild + removeChild should queue two records");

  const r0 = records[0];
  assert_equals(r0.type, "childList");
  assert_equals(r0.target, container);
  assert_equals(r0.addedNodes.length, 1);
  assert_equals(r0.addedNodes[0], b);
  assert_equals(r0.removedNodes.length, 0);
  assert_equals(r0.previousSibling, a);
  assert_equals(r0.nextSibling, null);

  const r1 = records[1];
  assert_equals(r1.type, "childList");
  assert_equals(r1.target, container);
  assert_equals(r1.addedNodes.length, 0);
  assert_equals(r1.removedNodes.length, 1);
  assert_equals(r1.removedNodes[0], b);
  assert_equals(r1.previousSibling, a);
  assert_equals(r1.nextSibling, null);

  const records2 = observer.takeRecords();
  assert_equals(records2.length, 0, "takeRecords() should clear the record queue");

  observer.disconnect();
}

test(
  mutation_observer_child_list_records_test,
  "MutationObserver childList records for appendChild/removeChild via takeRecords()"
);

function mutation_observer_attributes_records_test() {
  clear_children(document.body);

  const el = document.createElement("div");
  document.body.appendChild(el);

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(el, {
    attributes: true,
    attributeOldValue: true,
    attributeFilter: ["data-x"],
  });

  el.setAttribute("data-x", "a");
  el.setAttribute("data-y", "ignored");
  el.setAttribute("data-x", "b");

  const records = observer.takeRecords();
  assert_equals(records.length, 2, "attributeFilter should exclude data-y mutations");

  const r0 = records[0];
  assert_equals(r0.type, "attributes");
  assert_equals(r0.target, el);
  assert_equals(r0.attributeName, "data-x");
  assert_equals(r0.attributeNamespace, null);
  assert_equals(r0.oldValue, null);
  assert_equals(r0.addedNodes.length, 0);
  assert_equals(r0.removedNodes.length, 0);
  assert_equals(r0.previousSibling, null);
  assert_equals(r0.nextSibling, null);

  const r1 = records[1];
  assert_equals(r1.type, "attributes");
  assert_equals(r1.target, el);
  assert_equals(r1.attributeName, "data-x");
  assert_equals(r1.attributeNamespace, null);
  assert_equals(r1.oldValue, "a");

  observer.disconnect();
}

test(
  mutation_observer_attributes_records_test,
  "MutationObserver attributeOldValue + attributeFilter records via takeRecords()"
);

function mutation_observer_character_data_records_test() {
  clear_children(document.body);

  const text = document.createTextNode("old");
  document.body.appendChild(text);

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(text, { characterData: true, characterDataOldValue: true });

  text.data = "new";

  const records = observer.takeRecords();
  assert_equals(records.length, 1);

  const r0 = records[0];
  assert_equals(r0.type, "characterData");
  assert_equals(r0.target, text);
  assert_equals(r0.oldValue, "old");

  observer.disconnect();
}

test(
  mutation_observer_character_data_records_test,
  "MutationObserver characterDataOldValue records via takeRecords()"
);

function mutation_observer_disconnect_semantics_test() {
  clear_children(document.body);

  const container = document.createElement("div");
  document.body.appendChild(container);

  const observer = new MutationObserver(mutation_observer_dummy_callback);
  observer.observe(container, { childList: true });

  const child = document.createElement("span");
  container.appendChild(child);

  observer.disconnect();

  const records = observer.takeRecords();
  assert_equals(records.length, 0, "disconnect() should clear queued records");

  container.removeChild(child);
  const records2 = observer.takeRecords();
  assert_equals(records2.length, 0, "disconnect() should stop observation");
}

test(
  mutation_observer_disconnect_semantics_test,
  "MutationObserver disconnect() clears records and stops observation"
);

