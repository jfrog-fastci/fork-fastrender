// META: script=/resources/testharness.js
//
// Live Range updates for node insertion/removal.
//
// These tests cover:
// - insertion before a boundary point adjusts offsets (node inserting steps)
// - removing nodes before a boundary point adjusts offsets (live range pre-remove steps)
// - removing a node that contains the boundary point moves the boundary to (parent, index)
//
// https://dom.spec.whatwg.org/#concept-node-insert
// https://dom.spec.whatwg.org/#live-range-pre-remove-steps

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  // <div id=p><span id=a></span><span id=b></span></div>
  const p = document.createElement("div");
  p.id = "p";
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  p.appendChild(a);
  p.appendChild(b);
  document.body.appendChild(p);

  // Boundary points are in the parent using child offsets.
  // start: between a and b (offset 1)
  // end: after b (offset 2)
  const range = document.createRange();
  range.setStart(p, 1);
  range.setEnd(p, 2);
  assert_equals(range.startContainer, p);
  assert_equals(range.startOffset, 1);
  assert_equals(range.endContainer, p);
  assert_equals(range.endOffset, 2);

  // Insert a node *before* both boundary points (insert at index 0).
  const x = document.createElement("span");
  x.id = "x";
  p.insertBefore(x, a);

  // Both offsets should increase by 1 (they were both > insertion index).
  assert_equals(range.startContainer, p, "startContainer should stay the same");
  assert_equals(range.startOffset, 2, "startOffset should increase after insertion before the boundary");
  assert_equals(range.endContainer, p, "endContainer should stay the same");
  assert_equals(range.endOffset, 3, "endOffset should increase after insertion before the boundary");

  // Remove the inserted node; offsets should decrease back.
  p.removeChild(x);
  assert_equals(range.startOffset, 1, "startOffset should decrease after removing a node before the boundary");
  assert_equals(range.endOffset, 2, "endOffset should decrease after removing a node before the boundary");
}, "Live Range offsets update on insertBefore/removeChild around the boundary point in the parent");

test(() => {
  clear_children(document.body);

  // <div id=p><span id=a></span><span id=b></span></div>
  const p = document.createElement("div");
  p.id = "p";
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  p.appendChild(a);
  p.appendChild(b);
  document.body.appendChild(p);

  // Put the boundary point inside node `a` (collapsed).
  const range = document.createRange();
  range.setStart(a, 0);
  range.setEnd(a, 0);
  assert_equals(range.startContainer, a, "sanity: startContainer is the removed node");
  assert_equals(range.endContainer, a, "sanity: endContainer is the removed node");

  // Removing the node containing the boundary point should move the boundary point to
  // (parent, index) where the node was.
  p.removeChild(a);
  assert_equals(range.startContainer, p, "startContainer should move to the parent after removing the node");
  assert_equals(range.startOffset, 0, "startOffset should become the removed node's index");
  assert_equals(range.endContainer, p, "endContainer should move to the parent after removing the node");
  assert_equals(range.endOffset, 0, "endOffset should become the removed node's index");
  assert_true(range.collapsed, "range should remain collapsed after updating boundary points");
}, "Live Range boundary points move to (parent, index) when removing the node containing them");

