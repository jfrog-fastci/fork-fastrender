// META: script=/resources/testharness.js
//
// TreeWalker navigation methods (DOM Traversal).
//
// This is a curated subset intended to lock down spec-shaped TreeWalker navigation:
// - parent/child/sibling navigation
// - previous/next in tree order
// - FILTER_SKIP vs FILTER_REJECT semantics
// - re-entrancy guard while a filter callback is active
// - whatToShow bitmask short-circuit (filter not invoked for excluded nodeTypes)

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(tw.currentNode, root);

  assert_equals(tw.firstChild(), a);
  assert_equals(tw.currentNode, a);

  assert_equals(tw.nextSibling(), b);
  assert_equals(tw.currentNode, b);

  assert_equals(tw.nextSibling(), null, "nextSibling() at the end returns null");
  assert_equals(tw.currentNode, b, "currentNode is unchanged when returning null");

  assert_equals(tw.parentNode(), root);
  assert_equals(tw.currentNode, root);

  assert_equals(tw.parentNode(), null, "parentNode() at the root returns null");
  assert_equals(tw.currentNode, root, "currentNode is unchanged when returning null");
}, "TreeWalker basic navigation updates currentNode");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  const c = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);
  tw.currentNode = root;

  assert_equals(tw.lastChild(), c);
  assert_equals(tw.currentNode, c);

  assert_equals(tw.previousSibling(), b);
  assert_equals(tw.previousSibling(), a);
  assert_equals(tw.previousSibling(), null);
  assert_equals(tw.currentNode, a, "currentNode is unchanged when returning null");
}, "TreeWalker lastChild/previousSibling traversal");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  const a2 = document.createElement("i");
  a.appendChild(a1);
  a.appendChild(a2);
  const b = document.createElement("span");
  const b1 = document.createElement("b");
  b.appendChild(b1);
  const c = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);

  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), a1);
  assert_equals(tw.nextNode(), a2);
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), b1);
  assert_equals(tw.nextNode(), c);
  assert_equals(tw.nextNode(), null);
  assert_equals(tw.currentNode, c, "currentNode is unchanged when nextNode() returns null");

  assert_equals(tw.previousNode(), b1);
  assert_equals(tw.previousNode(), b);
  assert_equals(tw.previousNode(), a2);
  assert_equals(tw.previousNode(), a1);
  assert_equals(tw.previousNode(), a);
  assert_equals(tw.previousNode(), root);
  assert_equals(tw.previousNode(), null);
}, "TreeWalker nextNode/previousNode traverses in tree order");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  a.appendChild(a1);
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const filter = (node) => {
    if (node === a) return NodeFilter.FILTER_SKIP;
    return NodeFilter.FILTER_ACCEPT;
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.firstChild(), a1, "FILTER_SKIP should skip the node but still traverse into its children");
  assert_equals(tw.currentNode, a1);
  assert_equals(tw.nextSibling(), b, "nextSibling should traverse out of a skipped ancestor");
}, "TreeWalker FILTER_SKIP affects child/sibling traversal");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  a.appendChild(a1);
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const filter = (node) => {
    if (node === a) return NodeFilter.FILTER_REJECT;
    if (node === a1) assert_unreached("FILTER_REJECT should prune a rejected node's subtree");
    return NodeFilter.FILTER_ACCEPT;
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.firstChild(), b, "FILTER_REJECT should prune the rejected node's subtree");
}, "TreeWalker FILTER_REJECT prunes subtrees when traversing children");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  const b1 = document.createElement("b");
  b.appendChild(b1);
  const c = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  const filterSkipB = (node) => (node === b ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw1 = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filterSkipB);
  tw1.currentNode = a;
  assert_equals(tw1.nextSibling(), b1, "FILTER_SKIP on a sibling should descend into its children");

  const filterRejectB = (node) => (node === b ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT);
  const tw2 = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filterRejectB);
  tw2.currentNode = a;
  assert_equals(tw2.nextSibling(), c, "FILTER_REJECT on a sibling should skip its subtree");
}, "TreeWalker nextSibling FILTER_SKIP descends; FILTER_REJECT prunes");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  const a2 = document.createElement("i");
  a.appendChild(a1);
  a.appendChild(a2);
  const b = document.createElement("span");
  const b1 = document.createElement("b");
  b.appendChild(b1);
  const c = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);
  document.body.appendChild(root);

  const filter = (node) => (node === b ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT);
  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);

  assert_equals(tw.nextNode(), a);
  assert_equals(tw.nextNode(), a1);
  assert_equals(tw.nextNode(), a2);
  assert_equals(tw.nextNode(), b1, "FILTER_SKIP should skip b but still traverse into b1");
  assert_equals(tw.nextNode(), c);

  assert_equals(tw.previousNode(), b1);
  assert_equals(tw.previousNode(), a2, "previousNode should skip b when b is FILTER_SKIP");
}, "TreeWalker previousNode honors FILTER_SKIP (skipped nodes are not returned)");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const text1 = document.createTextNode("hello");
  const text2 = document.createTextNode("world");
  a.appendChild(text2);
  root.appendChild(text1);
  root.appendChild(a);
  document.body.appendChild(root);

  const calls = [];
  const filter = (node) => {
    calls.push(node);
    return NodeFilter.FILTER_ACCEPT;
  };

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, filter);
  assert_equals(tw.nextNode(), text1);
  assert_equals(tw.nextNode(), text2);
  assert_equals(tw.nextNode(), null);

  assert_equals(calls.length, 2, "filter callback should be invoked only for included nodeTypes");
  assert_equals(calls[0], text1);
  assert_equals(calls[1], text2);
  assert_equals(calls.indexOf(root), -1);
  assert_equals(calls.indexOf(a), -1);
}, "TreeWalker does not invoke the filter callback for nodes excluded by whatToShow");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  root.appendChild(a);
  document.body.appendChild(root);

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
        nested_name = e.name;
      }
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);
  assert_equals(tw.nextNode(), a);

  assert_true(did_reenter, "filter callback should have attempted a re-entrant traversal call");
  assert_true(nested_threw, "re-entrant traversal should throw");
  assert_equals(nested_name, "InvalidStateError");
}, "TreeWalker rejects re-entrant traversal calls from the filter callback");

