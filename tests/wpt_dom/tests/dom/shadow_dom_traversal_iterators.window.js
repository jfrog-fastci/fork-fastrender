// META: script=/resources/testharness.js
//
// NodeIterator / TreeWalker traversal should follow light DOM semantics:
// - When rooted at a non-ShadowRoot node (e.g. a shadow host element), traversal must not enter the
//   host's shadow tree and must not surface the internal ShadowRoot node stored under the host.
// - When rooted at a ShadowRoot, traversal should include shadow children normally.

function collect_node_iterator(it) {
  const out = [];
  while (true) {
    const n = it.nextNode();
    if (n === null) break;
    out.push(n);
  }
  return out;
}

function collect_tree_walker(tw) {
  const out = [];
  while (true) {
    const n = tw.nextNode();
    if (n === null) break;
    out.push(n);
  }
  return out;
}

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);
  const host = document.createElement("div");
  document.body.appendChild(host);

  const shadow = host.attachShadow({ mode: "open" });
  const inShadow = document.createElement("span");
  shadow.appendChild(inShadow);

  const a = document.createElement("a");
  const b = document.createElement("b");
  host.appendChild(a);
  host.appendChild(b);

  const it = document.createNodeIterator(host, NodeFilter.SHOW_ALL, null);
  const got = collect_node_iterator(it);
  assert_array_equals(got, [host, a, b], "NodeIterator rooted at host must not include ShadowRoot or shadow descendants");

  const tw = document.createTreeWalker(host, NodeFilter.SHOW_ELEMENT, null);
  const gotTw = collect_tree_walker(tw);
  assert_array_equals(gotTw, [a, b], "TreeWalker rooted at host must not include shadow descendants");
}, "Traversal rooted at a shadow host skips the shadow tree");

test(() => {
  clear_children(document.body);
  const host = document.createElement("div");
  document.body.appendChild(host);

  const shadow = host.attachShadow({ mode: "open" });
  const inShadow = document.createElement("span");
  shadow.appendChild(inShadow);

  const it = document.createNodeIterator(shadow, NodeFilter.SHOW_ELEMENT, null);
  const got = collect_node_iterator(it);
  assert_array_equals(got, [inShadow], "NodeIterator rooted at ShadowRoot should traverse shadow descendants");

  const tw = document.createTreeWalker(shadow, NodeFilter.SHOW_ELEMENT, null);
  const gotTw = collect_tree_walker(tw);
  assert_array_equals(gotTw, [inShadow], "TreeWalker rooted at ShadowRoot should traverse shadow descendants");
}, "Traversal rooted at ShadowRoot includes shadow tree nodes");
