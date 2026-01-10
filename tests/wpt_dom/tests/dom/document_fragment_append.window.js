// META: script=/resources/testharness.js
//
// DocumentFragment insertion semantics (appendChild).
//
// A DocumentFragment itself is never inserted into the tree. Instead, its children are moved into
// the target parent (in order) and the fragment becomes empty.
//
// This test also verifies that the vm-js DOM shim keeps `childNodes` behaving like a live NodeList:
// cached arrays should be mutated in-place when the DOM changes.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const fragment = document.createDocumentFragment();
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";

  fragment.appendChild(a);
  fragment.appendChild(b);

  // Cache live NodeLists before insertion.
  const parent_nodes = parent.childNodes;
  const frag_nodes = fragment.childNodes;

  assert_equals(parent_nodes.length, 0, "expected empty parent before insertion");
  assert_equals(frag_nodes.length, 2, "expected fragment to contain both children before insertion");
  assert_equals(frag_nodes[0], a, "expected fragment.childNodes[0] to be the first child");
  assert_equals(frag_nodes[1], b, "expected fragment.childNodes[1] to be the second child");

  const returned = parent.appendChild(fragment);
  assert_equals(returned, fragment, "appendChild should return the passed DocumentFragment");

  // `childNodes` should remain cached and update in-place.
  assert_equals(parent.childNodes, parent_nodes, "parent.childNodes should be cached");
  assert_equals(fragment.childNodes, frag_nodes, "fragment.childNodes should be cached");

  assert_equals(parent_nodes.length, 2, "expected fragment children to be inserted into parent");
  assert_equals(parent_nodes[0], a, "expected first inserted child to be a");
  assert_equals(parent_nodes[1], b, "expected second inserted child to be b");

  assert_equals(frag_nodes.length, 0, "expected fragment to be empty after insertion");
  assert_equals(fragment.parentNode, null, "DocumentFragment must not be inserted into the tree");

  assert_equals(a.parentNode, parent, "expected a.parentNode to be updated to the new parent");
  assert_equals(b.parentNode, parent, "expected b.parentNode to be updated to the new parent");
}, "DocumentFragment insertion via appendChild moves children and keeps childNodes live");

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const fragment = document.createDocumentFragment();
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  fragment.appendChild(a);
  fragment.appendChild(b);

  parent.appendChild(fragment);

  // Sanity: the inserted nodes should still be discoverable via selector APIs.
  assert_equals(parent.querySelector("#a"), a, "expected to find #a under the parent element");
  assert_equals(parent.querySelector("#b"), b, "expected to find #b under the parent element");
}, "Inserted DocumentFragment children are discoverable via querySelector");
