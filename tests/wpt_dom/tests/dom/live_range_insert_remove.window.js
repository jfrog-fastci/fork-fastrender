// META: script=/resources/testharness.js
//
// Live Range update behavior for Node insertion/removal.
//
// This targets the DOM Standard's "insert" and "remove" algorithms, which must update the
// start/end offsets of all live ranges when mutations affect their boundary points.
//
// Spec: https://dom.spec.whatwg.org/#concept-node-insert
// Spec: https://dom.spec.whatwg.org/#concept-node-remove

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  document.body.appendChild(parent);

  const r = document.createRange();

  // Collapsed after the 2nd child (offset 2 in the parent node).
  r.setStart(parent, 2);
  r.setEnd(parent, 2);
  assert_equals(r.startOffset, 2);
  assert_equals(r.endOffset, 2);

  const x = document.createElement("span");

  // Insert before `b` (index 1). Since the range boundary offsets are > 1, they must increment.
  parent.insertBefore(x, b);

  assert_equals(r.startContainer, parent);
  assert_equals(r.endContainer, parent);
  assert_equals(r.startOffset, 3);
  assert_equals(r.endOffset, 3);
}, "Live Range offsets increment when inserting a node before the boundary point");

test(() => {
  clear_children(document.body);

  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  document.body.appendChild(parent);

  const r = document.createRange();

  // Collapsed after the 2nd child (offset 2 in the parent node).
  r.setStart(parent, 2);
  r.setEnd(parent, 2);
  assert_equals(r.startOffset, 2);
  assert_equals(r.endOffset, 2);

  // Remove `a` (index 0). Since the range boundary offsets are > 0, they must decrement.
  parent.removeChild(a);

  assert_equals(r.startContainer, parent);
  assert_equals(r.endContainer, parent);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endOffset, 1);
}, "Live Range offsets decrement when removing a node before the boundary point");
