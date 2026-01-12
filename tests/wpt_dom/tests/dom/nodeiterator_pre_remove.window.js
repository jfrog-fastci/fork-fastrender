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
