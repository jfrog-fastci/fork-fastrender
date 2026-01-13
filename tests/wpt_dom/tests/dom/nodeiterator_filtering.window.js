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
  clear_children(document.body);

  const root = document.createElement("div");
  const c1 = document.createComment("c1");
  const e = document.createElement("span");
  const c2 = document.createComment("c2");
  e.appendChild(c2);
  root.appendChild(c1);
  root.appendChild(e);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_COMMENT, null);

  // Root and element nodes are skipped by whatToShow; the iterator advances until the first comment.
  assert_equals(it.nextNode(), c1);
  assert_equals(it.nextNode(), c2);
  assert_equals(it.nextNode(), null);
}, "NodeIterator whatToShow bitmask: SHOW_COMMENT skips elements but still reaches nested comments");

test(() => {
  const frag = document.createDocumentFragment();
  const a = document.createElement("div");
  const b = document.createElement("div");
  frag.appendChild(a);
  frag.appendChild(b);

  const what = NodeFilter.SHOW_DOCUMENT_FRAGMENT | NodeFilter.SHOW_ELEMENT;
  const it = document.createNodeIterator(frag, what, null);
  assert_equals(it.nextNode(), frag);
  assert_equals(it.nextNode(), a);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);
}, "NodeIterator whatToShow bitmask: SHOW_DOCUMENT_FRAGMENT can include a DocumentFragment root");

test(() => {
  const frag = document.createDocumentFragment();
  const a = document.createElement("div");
  frag.appendChild(a);

  const it = document.createNodeIterator(frag, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(it.nextNode(), a, "DocumentFragment root is skipped by whatToShow when SHOW_DOCUMENT_FRAGMENT is not set");
  assert_equals(it.nextNode(), null);
}, "NodeIterator whatToShow: skipping a DocumentFragment root still traverses descendants");

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

test(() => {
  const { root, a } = make_tree_with_text();

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode false, previousNode() flips it true without moving.
  assert_equals(it.previousNode(), a);
  assert_equals(it.referenceNode, a);
  assert_true(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode true, previousNode() moves to the first preceding node.
  assert_equals(it.previousNode(), root);
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode true, nextNode() flips it false without moving.
  assert_equals(it.nextNode(), root);
  assert_equals(it.referenceNode, root);
  assert_false(it.pointerBeforeReferenceNode);

  // With pointerBeforeReferenceNode false, nextNode() moves forward again.
  assert_equals(it.nextNode(), a);
  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator previousNode()/nextNode() move vs toggle based on pointerBeforeReferenceNode");

test(() => {
  const { root, a, a1, b } = make_tree_with_text();

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_equals(it.nextNode(), a1);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);

  // When nextNode() returns null, the iterator's state should remain unchanged.
  assert_equals(it.referenceNode, b);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator nextNode() returns null at the end without moving the iterator");

test(() => {
  const { root } = make_tree_with_text();

  let did_throw = false;
  const filter = () => {
    if (!did_throw) {
      did_throw = true;
      throw new Error("boom");
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_throws_js(Error, () => it.nextNode());
  assert_equals(it.referenceNode, root, "referenceNode should remain unchanged when the filter throws");
  assert_true(
    it.pointerBeforeReferenceNode,
    "pointerBeforeReferenceNode should remain unchanged when the filter throws"
  );
  assert_equals(it.nextNode(), root, "Traversal should continue after a filter exception");
}, "NodeIterator clears the re-entrancy guard when the filter throws");

test(() => {
  clear_children(document.body);

  const before = document.createElement("div");
  before.id = "before";
  const root = document.createElement("div");
  root.id = "root";
  const inside = document.createElement("div");
  inside.id = "inside";
  root.appendChild(inside);
  const after = document.createElement("div");
  after.id = "after";

  document.body.appendChild(before);
  document.body.appendChild(root);
  document.body.appendChild(after);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(
    it.previousNode(),
    null,
    "previousNode() at the start should not traverse to nodes outside the iterator root"
  );
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), inside);
  assert_equals(it.nextNode(), null, "nextNode() should stop at the iterator root boundary");
}, "NodeIterator does not traverse to preceding/following nodes outside its root subtree");

test(() => {
  const { root, a } = make_tree_with_text();

  const filter_skip_root = (node) =>
    node === root ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT;

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter_skip_root);
  assert_equals(it.nextNode(), a, "FILTER_SKIP on the iterator root should skip returning the root");
  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator filter can skip the root and still traverse descendants");

test(() => {
  const { root, a } = make_tree_with_text();

  let did_reenter = false;
  let nested_threw = false;
  let nested_name = "";
  let it = null;

  const filter = (node) => {
    // Trigger on the second accepted node so the iterator has already flipped
    // pointerBeforeReferenceNode to false.
    if (node === a && !did_reenter) {
      did_reenter = true;
      try {
        it.previousNode();
      } catch (e) {
        nested_threw = true;
        nested_name = e && e.name;
      }
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_true(did_reenter, "Filter callback should have attempted a re-entrant previousNode() call");
  assert_true(nested_threw, "Re-entrant previousNode() should throw");
  assert_equals(nested_name, "InvalidStateError");
}, "NodeIterator rejects re-entrant previousNode() calls from the filter callback (InvalidStateError)");

test(() => {
  const { root } = make_tree_with_text();

  const calls = [];
  const filter = (node) => {
    calls.push(node);
    return NodeFilter.FILTER_ACCEPT;
  };

  // whatToShow=0 excludes all node types (the n-th bit check always fails), so filtering returns
  // FILTER_SKIP without ever invoking the user filter callback. Traversal should therefore return
  // null and leave the iterator's state unchanged.
  const it = document.createNodeIterator(root, 0, filter);

  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);
  assert_equals(it.nextNode(), null);
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);
  assert_equals(calls.length, 0, "Filter callback should not be invoked when whatToShow excludes all nodes");
}, "NodeIterator whatToShow=0 excludes all nodes (nextNode returns null without moving and does not invoke the filter)");

test(() => {
  const { root } = make_tree_with_text();

  const calls = [];
  const filter = (node) => {
    calls.push(node);
    return NodeFilter.FILTER_SKIP;
  };

  // Even though every node is filtered out (SKIP), NodeIterator does not update its reference/pointer
  // unless an accepted node is found. When traversal reaches the end, nextNode() returns null
  // without moving the iterator.
  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);
  assert_equals(it.nextNode(), null);
  assert_equals(it.referenceNode, root);
  assert_true(it.pointerBeforeReferenceNode);

  assert_true(calls.length !== 0, "Filter callback should have been invoked for candidate nodes");
  assert_equals(calls[0], root, "First filter invocation should be for the root (which is a candidate node)");
}, "NodeIterator with a filter that never accepts returns null without moving the iterator");

test(() => {
  const { root, a } = make_tree_with_text();

  // FILTER_REJECT does not prune for NodeIterator; rejecting the root should still allow traversal to
  // its descendants.
  const reject_root = (node) =>
    node === root ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, reject_root);

  assert_equals(it.nextNode(), a, "Rejecting the root should not prevent visiting accepted descendants");
  assert_equals(it.referenceNode, a);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator FILTER_REJECT on the root behaves like FILTER_SKIP (no subtree pruning)");
