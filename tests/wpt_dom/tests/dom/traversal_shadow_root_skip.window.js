// META: script=/resources/testharness.js
//
// NodeIterator + TreeWalker traversal must not cross the internal host->ShadowRoot edge when
// walking the light DOM tree.

test(() => {
  const host = document.createElement("div");
  const shadow = host.attachShadow({ mode: "open" });
  shadow.appendChild(document.createElement("i"));
  const b = document.createElement("b");
  host.appendChild(b);

  const it = document.createNodeIterator(host, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(it.nextNode(), host);
  assert_equals(it.nextNode(), b);
  assert_equals(it.nextNode(), null, "NodeIterator must not traverse into shadow tree");
}, "NodeIterator skips internal ShadowRoot children when rooted at a host element");

test(() => {
  const host = document.createElement("div");
  const shadow = host.attachShadow({ mode: "open" });
  shadow.appendChild(document.createElement("i"));
  const b = document.createElement("b");
  host.appendChild(b);

  const tw = document.createTreeWalker(host, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(tw.nextNode(), b);
  assert_equals(tw.nextNode(), null, "TreeWalker must not traverse into shadow tree");
}, "TreeWalker skips internal ShadowRoot children when rooted at a host element");

