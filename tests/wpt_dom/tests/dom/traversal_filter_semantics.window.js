// META: script=/resources/testharness.js
//
// Additional DOM Traversal API coverage (TreeWalker/NodeIterator filter semantics).
//
// These tests expand on `dom/traversal_basics.window.js` to lock down:
// - NodeFilter.FILTER_REJECT subtree pruning for TreeWalker
// - NodeFilter.FILTER_SKIP child traversal without returning the skipped node for TreeWalker
// - NodeIterator whatToShow masking (e.g. SHOW_TEXT)
// - NodeIterator filter invocation order (including skipped nodes)

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  const a2 = document.createElement("i");
  a.appendChild(a1);
  a.appendChild(a2);
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, (node) => {
    if (node === a) {
      return NodeFilter.FILTER_REJECT;
    }
    return NodeFilter.FILTER_ACCEPT;
  });

  // Rejecting `a` should prune its subtree entirely (a1/a2 are not visited).
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), null);
}, "TreeWalker FILTER_REJECT prunes the rejected node's subtree (children not visited)");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("b");
  const a2 = document.createElement("i");
  a.appendChild(a1);
  a.appendChild(a2);
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, (node) => {
    if (node === a) {
      return NodeFilter.FILTER_SKIP;
    }
    return NodeFilter.FILTER_ACCEPT;
  });

  // Skipping `a` means it is not returned, but its children are traversed.
  assert_equals(tw.nextNode(), a1);
  assert_equals(tw.nextNode(), a2);
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), null);
}, "TreeWalker FILTER_SKIP traverses children but does not return the skipped node");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const text1 = document.createTextNode("t1");
  const span = document.createElement("span");
  const text2 = document.createTextNode("t2");
  span.appendChild(text2);
  const comment = document.createComment("c");
  const text3 = document.createTextNode("t3");

  root.appendChild(text1);
  root.appendChild(span);
  root.appendChild(comment);
  root.appendChild(text3);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_TEXT, null);

  const seen = [];
  while (true) {
    const node = it.nextNode();
    if (node === null) break;
    seen.push(node);
  }

  assert_array_equals(seen, [text1, text2, text3]);
  for (const node of seen) {
    assert_equals(node.nodeType, Node.TEXT_NODE);
  }
}, "NodeIterator whatToShow masks node types (SHOW_TEXT only returns text nodes)");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const a = document.createElement("span");
  const a1 = document.createElement("em");
  a.appendChild(a1);
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);
  document.body.appendChild(root);

  const filter_calls = [];
  const filter = (node) => {
    filter_calls.push(node);
    if (node === a) {
      return NodeFilter.FILTER_SKIP;
    }
    return NodeFilter.FILTER_ACCEPT;
  };

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, filter);

  const returned = [];
  while (true) {
    const node = it.nextNode();
    if (node === null) break;
    returned.push(node);
  }

  assert_array_equals(returned, [root, a1, b]);
  assert_array_equals(filter_calls, [root, a, a1, b]);
}, "NodeIterator filter is invoked for each candidate node in traversal order");

