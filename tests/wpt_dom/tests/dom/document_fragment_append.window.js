// META: script=/resources/testharness.js
//
// DocumentFragment insertion semantics (appendChild).
//
// A DocumentFragment itself is never inserted into the tree. Instead, its children are moved into
// the target parent (in order) and the fragment becomes empty.
//
// This test also verifies that the vm-js DOM shim keeps `childNodes` behaving like a live NodeList:
// cached arrays should be mutated in-place when the DOM changes.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var failed = false;

function fail(message) {
  if (failed) return;
  failed = true;
  report_fail(message);
}

function assert_true(cond, message) {
  if (failed) return;
  if (!cond) fail(message);
}

function assert_equals(actual, expected, message) {
  if (failed) return;
  if (actual !== expected) fail(message);
}

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function run() {
  var body = document.body;
  clear_children(body);

  var parent = document.createElement("div");
  body.appendChild(parent);

  var fragment = document.createDocumentFragment();
  var a = document.createElement("span");
  a.id = "a";
  var b = document.createElement("span");
  b.id = "b";

  fragment.appendChild(a);
  fragment.appendChild(b);

  // Cache live NodeLists before insertion.
  var parent_nodes = parent.childNodes;
  var frag_nodes = fragment.childNodes;

  assert_equals(parent_nodes.length, 0, "expected empty parent before insertion");
  assert_equals(frag_nodes.length, 2, "expected fragment to contain both children before insertion");
  assert_equals(frag_nodes[0], a, "expected fragment.childNodes[0] to be the first child");
  assert_equals(frag_nodes[1], b, "expected fragment.childNodes[1] to be the second child");

  if (failed) return;

  var returned = parent.appendChild(fragment);
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

  if (failed) return;

  // Sanity: the inserted nodes should still be discoverable via selector APIs.
  assert_true(parent.querySelector("#a") === a, "expected to find #a under the parent element");
  assert_true(parent.querySelector("#b") === b, "expected to find #b under the parent element");
}

run();

if (!failed) {
  report_pass();
}

