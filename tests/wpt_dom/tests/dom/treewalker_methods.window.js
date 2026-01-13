// META: script=/resources/testharness.js
//
// Extended DOM Traversal API coverage: TreeWalker method semantics, filtering behavior, and
// re-entrancy guards.
//
// Spec: https://dom.spec.whatwg.org/#interface-treewalker
// Filtering: https://dom.spec.whatwg.org/#concept-node-filter
//
// This is a curated, deterministic subset intended to exercise the full algorithmic surface of
// TreeWalker beyond `nextNode()` basics.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function make_tree() {
  clear_children(document.body);

  // Tree shape (tree order, element nodes only):
  // root
  //   a
  //     a1
  //     a2
  //   b
  //     b1
  //   c
  const root = document.createElement("div");
  root.id = "root";

  const a = document.createElement("div");
  a.id = "a";
  const a1 = document.createElement("div");
  a1.id = "a1";
  const a2 = document.createElement("div");
  a2.id = "a2";
  a.appendChild(a1);
  a.appendChild(a2);

  const b = document.createElement("div");
  b.id = "b";
  const b1 = document.createElement("div");
  b1.id = "b1";
  b.appendChild(b1);

  const c = document.createElement("div");
  c.id = "c";

  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  return { root, a, a1, a2, b, b1, c };
}

test(() => {
  const { root, a, a1, a2, b, b1 } = make_tree();
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(tw.currentNode, root);

  // root is the traversal boundary; parentNode() should return null and not move.
  assert_equals(tw.parentNode(), null);
  assert_equals(tw.currentNode, root);

  assert_equals(tw.firstChild(), a);
  assert_equals(tw.currentNode, a);

  assert_equals(tw.firstChild(), a1);
  assert_equals(tw.currentNode, a1);

  assert_equals(tw.nextSibling(), a2);
  assert_equals(tw.currentNode, a2);

  // a2 has no next sibling, and the first accepted ancestor is `a`, so nextSibling() returns null.
  assert_equals(tw.nextSibling(), null);
  assert_equals(tw.currentNode, a2);

  assert_equals(tw.parentNode(), a);
  assert_equals(tw.currentNode, a);

  assert_equals(tw.nextSibling(), b);
  assert_equals(tw.currentNode, b);

  assert_equals(tw.lastChild(), b1);
  assert_equals(tw.currentNode, b1);

  // b1 has no previous sibling, and `b` is accepted, so previousSibling() returns null.
  assert_equals(tw.previousSibling(), null);
  assert_equals(tw.currentNode, b1);
}, "TreeWalker navigation methods (parentNode/firstChild/lastChild/nextSibling/previousSibling) update currentNode per spec");

test(() => {
  const root = document.createElement("div");
  const tw = document.createTreeWalker(root, -1, null);
  assert_equals(tw.whatToShow, NodeFilter.SHOW_ALL);
}, "Document.createTreeWalker: whatToShow is converted using ToUint32 (-1 => SHOW_ALL)");

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

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(tw.previousNode(), null, "previousNode() at the root should not traverse outside the root");
  assert_equals(tw.currentNode, root);

  assert_equals(tw.nextNode(), inside);
  assert_equals(tw.nextNode(), null, "nextNode() should stop at the root boundary");
  assert_equals(tw.currentNode, inside, "currentNode remains unchanged when nextNode() returns null");
}, "TreeWalker does not traverse outside its root subtree");

test(() => {
  const { root, a, a1, a2, b, b1, c } = make_tree();
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);

  // nextNode() starts at currentNode and returns the first following descendant (not the root).
  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), a1);
  assert_equals(tw.nextNode(), a2);
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), b1);
  assert_equals(tw.nextNode(), c);
  assert_equals(tw.nextNode(), null);

  // previousNode() walks backwards in tree order, including returning the root.
  assert_equals(tw.currentNode, c, "currentNode remains unchanged when nextNode() returns null");

  assert_equals(tw.previousNode(), b1);
  assert_equals(tw.previousNode(), b);
  assert_equals(tw.previousNode(), a2);
  assert_equals(tw.previousNode(), a1);
  assert_equals(tw.previousNode(), a);
  assert_equals(tw.previousNode(), root);
  assert_equals(tw.previousNode(), null);
  assert_equals(tw.currentNode, root, "currentNode remains unchanged when previousNode() returns null");
}, "TreeWalker nextNode()/previousNode() traverse in tree order and respect the root boundary");

test(() => {
  const { root, a, a1, a2, b, b1 } = make_tree();
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);

  // The currentNode setter is not constrained beyond being a Node; it should affect subsequent
  // traversal operations immediately.
  tw.currentNode = b;
  assert_equals(tw.currentNode, b);
  assert_equals(tw.nextNode(), b1);

  tw.currentNode = a1;
  assert_equals(tw.nextNode(), a2);
  assert_equals(tw.parentNode(), a);
}, "TreeWalker currentNode setter affects subsequent traversal");

test(() => {
  const { root, a, a1, b } = make_tree();

  // FILTER_SKIP: skip the node itself but still descend into its children.
  const skip_a = (node) => (node === a ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw_skip = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_a);
  assert_equals(tw_skip.firstChild(), a1, "firstChild() should descend into skipped nodes");

  tw_skip.currentNode = root;
  assert_equals(tw_skip.nextNode(), a1, "nextNode() should descend into skipped nodes");

  // FILTER_REJECT: skip the node and its subtree.
  const reject_a = (node) => (node === a ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw_reject = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, reject_a);
  assert_equals(tw_reject.firstChild(), b, "firstChild() should skip rejected subtrees");

  tw_reject.currentNode = root;
  assert_equals(tw_reject.nextNode(), b, "nextNode() should skip rejected subtrees");
}, "TreeWalker filter return values: FILTER_SKIP descends into children; FILTER_REJECT prunes subtree");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  root.id = "root";
  const a = document.createElement("div");
  a.id = "a";
  root.appendChild(a);
  document.body.appendChild(root);

  const filter = (node) => (node === root ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.filter, filter);

  assert_equals(tw.nextNode(), a);
  assert_equals(tw.parentNode(), null, "parentNode() should not return the root when it is FILTER_SKIP");
  assert_equals(tw.currentNode, a);

  assert_equals(tw.previousNode(), null, "previousNode() should not return the root when it is FILTER_SKIP");
  assert_equals(tw.currentNode, a);
}, "TreeWalker respects FILTER_SKIP on the root (root is a traversal boundary but may be skipped)");

test(() => {
  const { root, a, a2, b } = make_tree();

  // When the previous sibling is FILTER_SKIP, previousSibling() should walk into its last accepted
  // descendant. When it is FILTER_REJECT, the subtree should be skipped entirely.
  const skip_a = (node) => (node === a ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw_skip = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_a);
  tw_skip.currentNode = b;
  assert_equals(tw_skip.previousSibling(), a2);

  const reject_a = (node) => (node === a ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw_reject = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, reject_a);
  tw_reject.currentNode = b;
  assert_equals(tw_reject.previousSibling(), null);
}, "TreeWalker traverse-siblings algorithm: FILTER_SKIP descends; FILTER_REJECT does not");

test(() => {
  const { root, a, b, b1, c } = make_tree();

  // traverse-siblings should walk into the first/last accepted descendant of a FILTER_SKIP sibling,
  // but treat FILTER_REJECT as a subtree prune.
  const skip_b = (node) => (node === b ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw_skip = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_b);
  tw_skip.currentNode = a;
  assert_equals(tw_skip.nextSibling(), b1);
  assert_equals(tw_skip.currentNode, b1);

  const reject_b = (node) => (node === b ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw_reject = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, reject_b);
  tw_reject.currentNode = a;
  assert_equals(tw_reject.nextSibling(), c);
  assert_equals(tw_reject.currentNode, c);
}, "TreeWalker nextSibling(): FILTER_SKIP descends into skipped siblings; FILTER_REJECT prunes their subtrees");

test(() => {
  const { root, a, a2, b } = make_tree();

  // previousNode() should descend into the last inclusive descendant of a FILTER_SKIP sibling,
  // but it should treat FILTER_REJECT as a subtree prune (so it will fall back to the nearest
  // accepted ancestor, which is the root in this tree).
  const skip_a = (node) => (node === a ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw_skip = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_a);
  tw_skip.currentNode = b;
  assert_equals(tw_skip.previousNode(), a2);

  const reject_a = (node) => (node === a ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw_reject = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, reject_a);
  tw_reject.currentNode = b;
  assert_equals(tw_reject.previousNode(), root);
}, "TreeWalker previousNode(): FILTER_SKIP descends into skipped siblings; FILTER_REJECT prunes subtrees");

test(() => {
  const { root, a, a2, b } = make_tree();

  // If an ancestor is not FILTER_ACCEPT, the traverse-siblings algorithm may walk up and return
  // that ancestor's next sibling.
  const skip_a = (node) => (node === a ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_a);

  tw.currentNode = a2;
  assert_equals(tw.nextSibling(), b);
  assert_equals(tw.currentNode, b);
}, "TreeWalker nextSibling(): may traverse to an ancestor's sibling when the ancestor is not FILTER_ACCEPT");

test(() => {
  clear_children(document.body);

  // Tree shape:
  // root
  //   a
  //   b
  //   c
  //     c1
  const root = document.createElement("div");
  root.id = "root";
  const a = document.createElement("div");
  a.id = "a";
  const b = document.createElement("div");
  b.id = "b";
  const c = document.createElement("div");
  c.id = "c";
  const c1 = document.createElement("div");
  c1.id = "c1";
  c.appendChild(c1);
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  const skip_c = (node) => (node === c ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw_skip = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, skip_c);
  assert_equals(tw_skip.lastChild(), c1, "lastChild() should descend into skipped nodes");
  assert_equals(tw_skip.currentNode, c1);
  assert_equals(tw_skip.parentNode(), root, "parentNode() should walk to the first accepted ancestor");
  assert_equals(tw_skip.currentNode, root);

  const reject_c = (node) => (node === c ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw_reject = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, reject_c);
  assert_equals(tw_reject.lastChild(), b, "lastChild() should skip rejected subtrees");
  assert_equals(tw_reject.currentNode, b);
}, "TreeWalker lastChild() and parentNode(): FILTER_SKIP descends, FILTER_REJECT prunes, and parentNode() finds the first accepted ancestor");

test(() => {
  const { root, a } = make_tree();

  let did_reenter = false;
  let nested_threw = false;
  let nested_name = "";
  let tw = null;

  const filter = () => {
    if (!did_reenter) {
      did_reenter = true;
      try {
        tw.nextNode();
      } catch (e) {
        nested_threw = true;
        nested_name = e && e.name;
      }
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(tw.nextNode(), a);
  assert_true(did_reenter, "Filter callback should have attempted a re-entrant traversal call");
  assert_true(nested_threw, "Re-entrant nextNode() should throw");
  assert_equals(nested_name, "InvalidStateError");
}, "TreeWalker rejects re-entrant nextNode() calls from the filter callback (InvalidStateError)");

test(() => {
  const { root, a, a1 } = make_tree();

  let did_reenter = false;
  let nested_threw = false;
  let nested_name = "";
  let tw = null;

  const filter = (node) => {
    // Trigger on a node where `currentNode` is not the root, so parentNode() will run filtering.
    if (node === a1 && !did_reenter) {
      did_reenter = true;
      try {
        tw.parentNode();
      } catch (e) {
        nested_threw = true;
        nested_name = e && e.name;
      }
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), a1);
  assert_true(did_reenter, "Filter callback should have attempted a re-entrant parentNode() call");
  assert_true(nested_threw, "Re-entrant parentNode() should throw");
  assert_equals(nested_name, "InvalidStateError");
}, "TreeWalker rejects re-entrant parentNode() calls from the filter callback (InvalidStateError)");

test(() => {
  const { root, a } = make_tree();

  let did_throw = false;
  const filter = () => {
    if (!did_throw) {
      did_throw = true;
      throw new Error("boom");
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_throws_js(Error, () => tw.nextNode());
  assert_equals(tw.currentNode, root, "currentNode should remain unchanged when the filter throws");
  assert_equals(tw.nextNode(), a, "Traversal should continue after a filter exception");
}, "TreeWalker clears the re-entrancy guard when the filter throws");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const text = document.createTextNode("hello");
  const a = document.createElement("span");
  root.appendChild(text);
  root.appendChild(a);
  document.body.appendChild(root);

  const calls = [];
  const filter = (node) => {
    calls.push(node);
    return NodeFilter.FILTER_ACCEPT;
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), null);

  assert_true(calls.indexOf(a) !== -1, "Filter should be invoked for nodes included by whatToShow");
  assert_equals(
    calls.indexOf(text),
    -1,
    "Filter should not be invoked for nodes excluded by whatToShow (text)"
  );
}, "TreeWalker does not invoke the filter callback for nodes excluded by whatToShow");

test(() => {
  const { root, a, a1, a2, b, b1, c } = make_tree();

  const calls = [];
  const filter = {
    acceptNode(node) {
      calls.push(this);
      calls.push(node);
      return node === a1 ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP;
    },
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.nextNode(), a1);
  assert_equals(tw.nextNode(), null);

  // Ensure acceptNode is called with `this` bound to the filter object and in tree order for
  // candidate nodes.
  assert_array_equals(calls, [
    filter,
    a,
    filter,
    a1,
    filter,
    a2,
    filter,
    b,
    filter,
    b1,
    filter,
    c,
  ]);
}, "TreeWalker supports NodeFilter objects with an acceptNode() method (including this-binding)");
