// META: script=/resources/testharness.js
//
// NodeIterator live update behavior when nodes are removed.
//
// This targets the DOM Standard's "NodeIterator pre-remove steps" algorithm, which updates an
// iterator's referenceNode and pointerBeforeReferenceNode when an ancestor of the reference node
// is removed.
//
// Spec: https://dom.spec.whatwg.org/#nodeiterator-pre-removing-steps

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");

  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);

  const b = document.createElement("span");

  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  let it;
  try {
    it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);
  } catch (e) {
    assert_unreached(`createNodeIterator threw: ${e && e.name}`);
  }

  // Move the iterator into the subtree that will be removed.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1
  // previousNode when pointerBeforeReferenceNode is false flips it to true without moving.
  it.previousNode(); // a1

  assert_equals(it.referenceNode, a1);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing `a` should move the reference to the first following node outside the removed subtree.
  root.removeChild(a);

  assert_equals(it.referenceNode, b);
  assert_true(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: pointer-before-reference moves reference to the first following node outside removed subtree");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");

  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);

  root.appendChild(a);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);

  // Move into `a`'s subtree and flip pointerBeforeReferenceNode to true without moving.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1
  it.previousNode(); // a1 (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, a1);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing `a` leaves no following node outside the removed subtree, so the iterator should:
  // 1) flip pointerBeforeReferenceNode to false, then
  // 2) set referenceNode to the removed node's parent (since it had no previous sibling).
  root.removeChild(a);

  assert_equals(it.referenceNode, root);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: when there is no following node, pointer-before-reference falls back to the removed node's parent and flips pointerBeforeReferenceNode false");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);
  it.nextNode(); // root
  it.nextNode(); // a

  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);

  // Removing a node that is not an inclusive ancestor of the reference must not change the iterator.
  root.removeChild(b);
  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: removing a non-ancestor node does not affect referenceNode/pointerBeforeReferenceNode");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");

  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);

  const b = document.createElement("span");
  const b1 = document.createElement("span");
  b.appendChild(b1);

  const c = document.createElement("span");

  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  let it;
  try {
    it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);
  } catch (e) {
    assert_unreached(`createNodeIterator threw: ${e && e.name}`);
  }

  // Advance until the reference is within `b`'s subtree.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1
  it.nextNode(); // b
  it.nextNode(); // b1

  assert_equals(it.referenceNode, b1);
  assert_false(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode false, removing the ancestor should move the reference to
  // the previous sibling's last inclusive descendant (a1).
  root.removeChild(b);

  assert_equals(it.referenceNode, a1);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: pointer-after-reference moves reference to previous sibling's last inclusive descendant");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");

  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);

  const b = document.createElement("span");
  const b1 = document.createElement("span");
  b.appendChild(b1);

  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);

  // Advance to b1, then flip pointerBeforeReferenceNode to true without moving.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1
  it.nextNode(); // b
  it.nextNode(); // b1
  it.previousNode(); // b1 (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, b1);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing `b` leaves no following node. Since `b` has a previous sibling, the reference should
  // fall back to that previous sibling's last inclusive descendant (a1), and pointerBeforeReferenceNode
  // should flip to false.
  root.removeChild(b);

  assert_equals(it.referenceNode, a1);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: pointer-before-reference with no following node falls back to previous sibling's last inclusive descendant");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");

  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);
  root.appendChild(a);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);

  // Advance to the leaf so pointerBeforeReferenceNode is false.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1

  assert_equals(it.referenceNode, a1);
  assert_false(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode false and no previous sibling, reference should fall back to
  // the removed node's parent, while pointerBeforeReferenceNode remains false.
  root.removeChild(a);

  assert_equals(it.referenceNode, root);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: pointer-after-reference with no previous sibling falls back to the removed node's parent");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("span");
  a.appendChild(a1);
  root.appendChild(a);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ALL, null);

  // Move into the root's subtree so reference is a descendant.
  it.nextNode(); // root
  it.nextNode(); // a
  it.nextNode(); // a1
  it.previousNode(); // a1 (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, a1);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing the iterator root should *not* run NodeIterator pre-remove steps for this iterator,
  // per spec (early return when toBeRemovedNode === iterator.root).
  document.body.removeChild(root);

  assert_equals(it.referenceNode, a1);
  assert_true(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove steps: removing the iterator root does not update referenceNode/pointerBeforeReferenceNode");
