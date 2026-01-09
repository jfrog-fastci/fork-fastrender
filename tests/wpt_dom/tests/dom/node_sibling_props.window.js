// META: script=/resources/testharness.js
//
// Basic `firstChild`/`lastChild`/`previousSibling`/`nextSibling` behavior for the vm-js DOM shim.
//
// These are data properties in the current harness (no accessor support), so the host must update
// them whenever the tree structure changes.

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

  var a = document.createElement("span");
  a.id = "a";
  var b = document.createElement("span");
  b.id = "b";

  parent.appendChild(a);
  parent.appendChild(b);

  assert_equals(parent.firstChild, a, "firstChild should be the first appended node");
  assert_equals(parent.lastChild, b, "lastChild should be the last appended node");

  assert_equals(a.previousSibling, null, "first child's previousSibling should be null");
  assert_equals(a.nextSibling, b, "a.nextSibling should be b");
  assert_equals(b.previousSibling, a, "b.previousSibling should be a");
  assert_equals(b.nextSibling, null, "last child's nextSibling should be null");

  if (failed) return;

  parent.removeChild(a);

  assert_equals(a.parentNode, null, "removed child's parentNode should be null");
  assert_equals(a.previousSibling, null, "removed child's previousSibling should be cleared");
  assert_equals(a.nextSibling, null, "removed child's nextSibling should be cleared");

  assert_equals(parent.firstChild, b, "firstChild should update after removal");
  assert_equals(parent.lastChild, b, "lastChild should update after removal");
  assert_equals(b.previousSibling, null, "only child's previousSibling should be null");
  assert_equals(b.nextSibling, null, "only child's nextSibling should be null");

  if (failed) return;

  parent.appendChild(a);

  assert_equals(parent.firstChild, b, "firstChild should remain b after re-append");
  assert_equals(parent.lastChild, a, "lastChild should become a after re-append");

  assert_equals(b.previousSibling, null, "b.previousSibling should still be null");
  assert_equals(b.nextSibling, a, "b.nextSibling should become a");
  assert_equals(a.previousSibling, b, "a.previousSibling should become b");
  assert_equals(a.nextSibling, null, "a.nextSibling should be null at end");

  if (failed) return;

  // DocumentFragment insertion should also update sibling pointers for moved children.
  var fragment = document.createDocumentFragment();
  var c = document.createElement("span");
  c.id = "c";
  var d = document.createElement("span");
  d.id = "d";
  fragment.appendChild(c);
  fragment.appendChild(d);
  parent.appendChild(fragment);

  assert_equals(parent.lastChild, d, "lastChild should be the last moved fragment child");
  assert_equals(c.previousSibling, a, "c.previousSibling should be a after fragment insertion");
  assert_equals(c.nextSibling, d, "c.nextSibling should be d after fragment insertion");
  assert_equals(d.previousSibling, c, "d.previousSibling should be c after fragment insertion");
  assert_equals(d.nextSibling, null, "d.nextSibling should be null at end");
}

run();

if (!failed) {
  report_pass();
}

