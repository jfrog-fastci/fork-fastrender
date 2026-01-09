// META: script=/resources/testharness.js

test(() => {
  const text = document.createTextNode("hi");
  assert_true(text instanceof Text);
  assert_true(text instanceof Node);
  assert_equals(text.nodeType, Node.TEXT_NODE);
  assert_equals(text.nodeName, "#text");
  assert_equals(text.data, "hi");

  text.data = "a&b<>";
  assert_equals(text.data, "a&b<>");

  text.textContent = "x";
  assert_equals(text.data, "x");
}, "Text node basics (instanceof + data + textContent)");

test(() => {
  const host = document.createElement("div");
  const text = document.createTextNode("a&b<>");
  host.appendChild(text);
  assert_equals(host.innerHTML, "a&amp;b&lt;&gt;");
  assert_equals(host.outerHTML, "<div>a&amp;b&lt;&gt;</div>");
  assert_equals(host.textContent, "a&b<>");
}, "Text nodes serialize via innerHTML/outerHTML and contribute to textContent");

test(() => {
  const el = document.createElement("div");
  assert_false(el.isConnected);
  assert_equals(el.ownerDocument, document);

  document.body.appendChild(el);
  assert_true(el.isConnected);

  const text = document.createTextNode("x");
  assert_false(text.isConnected);
  el.appendChild(text);
  assert_true(text.isConnected);
  assert_equals(text.ownerDocument, document);

  const frag = document.createDocumentFragment();
  assert_false(frag.isConnected);
  assert_equals(frag.ownerDocument, document);

  assert_equals(document.ownerDocument, null);
  assert_true(document.isConnected);
}, "ownerDocument + isConnected basics");

test(() => {
  const host = document.createElement("div");
  host.innerHTML = "<span>hi</span><span>there</span>";
  assert_equals(host.textContent, "hithere");
}, "Element.textContent concatenates descendant text");

test(() => {
  const host = document.createElement("div");
  host.innerHTML = "<span>one</span><span>two</span>";

  host.textContent = "a&b<>";
  assert_equals(host.innerHTML, "a&amp;b&lt;&gt;");
  assert_equals(host.textContent, "a&b<>");
  assert_equals(host.firstChild.nodeType, Node.TEXT_NODE);
}, "Setting Element.textContent replaces children with a text node");

