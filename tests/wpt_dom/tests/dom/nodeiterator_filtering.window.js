// META: script=/resources/testharness.js
//
// Extended DOM Traversal API coverage: NodeIterator edge cases for pointer toggling, whatToShow
// bitmasking, filter semantics (SKIP vs REJECT), detach() behavior, and NodeFilter object filters.
//
// Spec: https://dom.spec.whatwg.org/#interface-nodeiterator

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function make_tree_with_text() {
  clear_children(document.body);

  // Tree order under root:
  // root
  //   a
  //     t1 (Text)
  //     a1
  //       t2 (Text)
  //   b
  //     t3 (Text)
  const root = document.createElement("div");
  root.id = "root";

  const a = document.createElement("div");
  a.id = "a";
  const t1 = document.createTextNode("t1");
  const a1 = document.createElement("div");
  a1.id = "a1";
  const t2 = document.createTextNode("t2");
  a1.appendChild(t2);
  a.appendChild(t1);
  a.appendChild(a1);

  const b = document.createElement("div");
  b.id = "b";
  const t3 = document.createTextNode("t3");
  b.appendChild(t3);

  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  return { root, a, a1, b, t1, t2, t3 };
}

test(() => {
  const { root } = make_tree_with_text();

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);

  // NodeIterator starts "before root".
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);

  // nextNode() when pointerBeforeReferenceNode is true returns the current reference and flips it.
  assert_equals(it.nextNode(), root);
  assert_equals(it.referenceNode, root);
  assert_false(it.pointerBeforeReferenceNode);

  // previousNode() when pointerBeforeReferenceNode is false returns the current reference and flips
  // it to true without moving.
  assert_equals(it.previousNode(), root);
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);

  // A second previousNode() now attempts to move to a preceding node, which doesn't exist.
  assert_equals(it.previousNode(), null);
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);
}, "NodeIterator previousNode() toggles pointerBeforeReferenceNode without moving when it is false");

test(() => {
  const { root, a, a1, b, t1, t2, t3 } = make_tree_with_text();

  const what = NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT;
  const it = document.createNodeIterator(root, what, null);

  // Includes elements and text nodes in tree order, starting with the root.
  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_equals(it.nextNode(), t1);
  assert_equals(it.nextNode(), a1);
  assert_equals(it.nextNode(), t2);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), t3);
  assert_equals(it.nextNode(), null);
}, "NodeIterator whatToShow bitmask: SHOW_ELEMENT | SHOW_TEXT yields elements and text nodes");

test(() => {
  const { root, t1, t2, t3 } = make_tree_with_text();

  const it = document.createNodeIterator(root, NodeFilter.SHOW_TEXT, null);

  // Root and element nodes are skipped by whatToShow; the iterator advances until the first text.
  assert_equals(it.nextNode(), t1);
  assert_equals(it.nextNode(), t2);
  assert_equals(it.nextNode(), t3);
  assert_equals(it.nextNode(), null);
}, "NodeIterator whatToShow bitmask: SHOW_TEXT skips elements (including the root)");

test(() => {
  const { root, a, a1, b } = make_tree_with_text();

  // NodeIterator does not treat FILTER_REJECT as a subtree prune; it keeps scanning until an
  // accepted node is found.
  const filter_reject_a = (node) => (node === a ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter_reject_a);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a1, "FILTER_REJECT should not prune descendants for NodeIterator");
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);
}, "NodeIterator FILTER_REJECT does not prune descendants (unlike TreeWalker)");

test(() => {
  const { root, a, a1, b } = make_tree_with_text();

  const filter_skip_a = (node) => (node === a ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter_skip_a);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a1, "FILTER_SKIP and FILTER_REJECT should behave the same for NodeIterator");
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);
}, "NodeIterator FILTER_SKIP behaves like FILTER_REJECT (no subtree pruning)");

test(() => {
  const { root, a, b } = make_tree_with_text();

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(it.nextNode(), root);
  assert_equals(it.detach(), undefined, "detach() should be a no-op returning undefined");
  assert_equals(it.nextNode(), a, "Traversal should continue after detach()");
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);
}, "NodeIterator.detach() is a no-op and does not break traversal");

test(() => {
  const { root, a1 } = make_tree_with_text();

  // NodeFilter is a callback interface; passing a plain object with `acceptNode` should work.
  const filter = {
    acceptNode(node) {
      return node === a1 ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP;
    },
  };

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(it.nextNode(), a1);
  assert_equals(it.nextNode(), null);
}, "NodeIterator supports NodeFilter objects with an acceptNode() method");

