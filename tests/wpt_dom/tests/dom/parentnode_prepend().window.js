// META: script=/resources/testharness.js
//
// Modern DOM convenience mutation APIs (ParentNode.prepend).

function clear_children(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const existing = document.createElement("span");
  existing.id = "existing";
  parent.appendChild(existing);

  const inserted = document.createElement("span");
  inserted.id = "inserted";

  const ret = parent.prepend("a", inserted, "c");
  assert_equals(ret, undefined, "prepend() should return undefined");

  assert_equals(parent.childNodes.length, 4, "expected four childNodes after prepend()");
  assert_equals(parent.childNodes[0].nodeType, Node.TEXT_NODE, "expected first child to be Text");
  assert_equals(parent.childNodes[0].data, "a", "expected first Text node data");
  assert_equals(parent.childNodes[1], inserted, "expected inserted element");
  assert_equals(parent.childNodes[2].nodeType, Node.TEXT_NODE, "expected third child to be Text");
  assert_equals(parent.childNodes[2].data, "c", "expected third Text node data");
  assert_equals(parent.childNodes[3], existing, "expected existing child to be last");
}, "ParentNode.prepend inserts nodes before existing children and converts strings to Text");

