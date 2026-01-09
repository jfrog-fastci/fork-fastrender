// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  assert_equals(el.tagName, "DIV");

  el.innerHTML = "<span>hi</span>";
  assert_equals(el.innerHTML, "<span>hi</span>");
  assert_equals(el.outerHTML, "<div><span>hi</span></div>");
}, "createElement + Element.innerHTML/outerHTML");

test(() => {
  const frag = document.createDocumentFragment();
  const child = document.createElement("div");
  const returned = frag.appendChild(child);
  assert_equals(returned, child, "appendChild should return the inserted node");
}, "document.createDocumentFragment");

test(() => {
  const host = document.createElement("div");
  const frag = document.createDocumentFragment();

  const a = document.createElement("span");
  a.innerHTML = "a";
  const b = document.createElement("span");
  b.innerHTML = "b";

  frag.appendChild(a);
  frag.appendChild(b);
  host.appendChild(frag);

  assert_equals(host.innerHTML, "<span>a</span><span>b</span>");

  // Fragment insertion should be by "moving children"; appending again is a no-op.
  host.appendChild(frag);
  assert_equals(host.innerHTML, "<span>a</span><span>b</span>");
}, "Node.appendChild supports DocumentFragment insertion semantics");

test(() => {
  const container = document.createElement("div");
  const child = document.createElement("span");
  child.innerHTML = "x";
  container.appendChild(child);

  child.outerHTML = "<p>y</p><p>z</p>";
  assert_equals(container.innerHTML, "<p>y</p><p>z</p>");
}, "Element.outerHTML setter replaces the node in its parent");
