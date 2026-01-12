// META: script=/resources/testharness.js
//
// Baseline DOM Traversal API behavior: NodeIterator, TreeWalker, and NodeFilter.
//
// This is a curated subset intended to lock down the expected initialization state and
// traversal order as specified by the DOM Standard.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  assert_equals(typeof document.createNodeIterator, "function");
  assert_equals(typeof document.createTreeWalker, "function");
  assert_true(
    typeof NodeFilter === "function" || typeof NodeFilter === "object",
    "NodeFilter should be exposed as a global interface object"
  );

  assert_equals(NodeFilter.FILTER_ACCEPT, 1);
  assert_equals(NodeFilter.FILTER_REJECT, 2);
  assert_equals(NodeFilter.FILTER_SKIP, 3);

  assert_equals(NodeFilter.SHOW_ALL, 0xFFFFFFFF);
  assert_equals(NodeFilter.SHOW_ELEMENT, 0x1);
  assert_equals(NodeFilter.SHOW_TEXT, 0x4);
  assert_equals(NodeFilter.SHOW_COMMENT, 0x80);
  assert_equals(NodeFilter.SHOW_DOCUMENT, 0x100);
  assert_equals(NodeFilter.SHOW_DOCUMENT_FRAGMENT, 0x400);
}, "Traversal API surface + NodeFilter constants");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(it.root, root);
  assert_equals(it.referenceNode, root);
  assert_equals(it.pointerBeforeReferenceNode, true);
  assert_equals(it.whatToShow, NodeFilter.SHOW_ELEMENT);
  assert_equals(it.filter, null);

  // NodeIterator starts "before root", so the first nextNode() returns the root.
  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);
}, "NodeIterator initializes state and traverses in tree order");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(tw.root, root);
  assert_equals(tw.currentNode, root);
  assert_equals(tw.whatToShow, NodeFilter.SHOW_ELEMENT);
  assert_equals(tw.filter, null);

  // TreeWalker.nextNode() does not return the root; it returns the first descendant.
  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), null);
}, "TreeWalker initializes state and traverses descendants in tree order");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  body.appendChild(root);

  const calls = [];
  const filter = (node) => {
    calls.push(node);
    return NodeFilter.FILTER_ACCEPT;
  };

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(it.filter, filter);

  assert_equals(it.nextNode(), root);
  assert_equals(it.nextNode(), a);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null);

  assert_equals(calls.length, 3, "Filter callback should be invoked for each visited node");
  assert_equals(calls[0], root);
  assert_equals(calls[1], a);
  assert_equals(calls[2], b);
}, "NodeIterator invokes a filter callback during traversal");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  root.appendChild(a);
  body.appendChild(root);

  let did_reenter = false;
  let nested_threw = false;
  let nested_name = "";
  let it = null;

  const filter = () => {
    if (!did_reenter) {
      did_reenter = true;
      try {
        it.nextNode();
      } catch (e) {
        nested_threw = true;
        nested_name = e.name;
      }
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(it.nextNode(), root);

  assert_true(did_reenter, "Filter callback should have attempted a re-entrant traversal call");
  assert_true(nested_threw, "Re-entrant nextNode() should throw");
  assert_equals(nested_name, "InvalidStateError");
}, "NodeIterator rejects re-entrant nextNode() calls from the filter callback");
