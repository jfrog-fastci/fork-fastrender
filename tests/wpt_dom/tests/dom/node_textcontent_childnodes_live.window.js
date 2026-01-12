// META: script=/resources/testharness.js
//
// Regression test: `Node.textContent` must replace children via DOM mutation primitives so cached
// `childNodes` NodeLists stay live.

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "<span></span><b></b>";

  const nodes = el.childNodes;
  assert_equals(nodes.length, 2, "precondition: childNodes should start with two children");

  el.textContent = "x";

  // The previously-cached NodeList must update without needing to re-access `el.childNodes`.
  assert_equals(nodes.length, 1, "cached childNodes must update after textContent setter");
  assert_equals(nodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(nodes[0].data, "x");

  // `childNodes` must preserve identity.
  assert_equals(el.childNodes, nodes);
}, "Node.textContent replaces children and keeps cached childNodes live");

