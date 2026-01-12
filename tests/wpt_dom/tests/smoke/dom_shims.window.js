// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof document.createElement, "function");
  const el = document.createElement("div");
  assert_equals(el.tagName, "DIV");
  assert_true(el instanceof Element, "createElement should return an Element");
  assert_true(el instanceof Node, "Element should inherit from Node");

  el.id = "root";
  el.className = "a b";
  assert_equals(el.getAttribute("id"), "root");
  assert_equals(el.getAttribute("class"), "a b");

  el.innerHTML = '<span id="x" class="y">hi</span>';
  assert_equals(el.innerHTML, '<span id="x" class="y">hi</span>');
  assert_equals(
    el.outerHTML,
    '<div id="root" class="a b"><span id="x" class="y">hi</span></div>'
  );

  assert_equals(el.childNodes.length, 1, "innerHTML should populate Element.childNodes");
  const span = el.childNodes[0];
  assert_true(span instanceof Element);
  assert_equals(span.tagName, "SPAN");
  assert_equals(span.id, "x");
  assert_equals(span.className, "y");
  assert_equals(el.firstChild, span);
  assert_equals(el.lastChild, span);
  assert_equals(span.previousSibling, null);
  assert_equals(span.nextSibling, null);
  assert_equals(span.childNodes.length, 1);
  const text = span.childNodes[0];
  assert_true(text instanceof Text);
  assert_equals(text.data, "hi");
  assert_equals(span.firstChild, text);
  assert_equals(span.lastChild, text);
  assert_equals(text.previousSibling, null);
  assert_equals(text.nextSibling, null);

  el.removeAttribute("id");
  assert_equals(el.id, "");
}, "createElement + Element.innerHTML/outerHTML");

test(() => {
  assert_equals(typeof document.createDocumentFragment, "function");
  const frag = document.createDocumentFragment();
  assert_true(
    frag instanceof DocumentFragment,
    "createDocumentFragment should return a DocumentFragment"
  );
  assert_true(frag instanceof Node, "DocumentFragment should inherit from Node");
  const child = document.createElement("div");
  const returned = frag.appendChild(child);
  assert_equals(returned, child, "appendChild should return the inserted node");
}, "document.createDocumentFragment");

test(() => {
  assert_equals(typeof document.createTextNode, "function");
  const text = document.createTextNode("hi");
  assert_true(text instanceof Text, "createTextNode should return a Text");
  assert_true(text instanceof Node, "Text should inherit from Node");
  assert_equals(text.data, "hi");

  text.data = "a&b<>";
  assert_equals(text.data, "a&b<>");

  const el = document.createElement("div");
  el.appendChild(text);
  assert_equals(el.innerHTML, "a&amp;b&lt;&gt;");
  assert_equals(el.outerHTML, "<div>a&amp;b&lt;&gt;</div>");
}, "document.createTextNode + Text.data");

test(() => {
  assert_equals(document.textContent, null, "Document.textContent should be null");

  const el = document.createElement("div");
  assert_equals(el.textContent, "");
  el.innerHTML = "<span>hi</span><span>there</span>";
  assert_equals(el.textContent, "hithere");

  el.textContent = "a&b<>";
  assert_equals(el.innerHTML, "a&amp;b&lt;&gt;");
  assert_equals(el.textContent, "a&b<>");

  const text = document.createTextNode("x");
  text.textContent = "y";
  assert_equals(text.data, "y");
}, "Node.textContent shims");

test(() => {
  assert_equals(document.nodeType, Node.DOCUMENT_NODE);
  assert_equals(document.nodeName, "#document");
  assert_equals(document.ownerDocument, null);
  assert_true(document.isConnected);

  const frag = document.createDocumentFragment();
  assert_equals(frag.nodeType, Node.DOCUMENT_FRAGMENT_NODE);
  assert_equals(frag.nodeName, "#document-fragment");
  assert_equals(frag.ownerDocument, document);
  assert_false(frag.isConnected);

  const el = document.createElement("div");
  assert_equals(el.nodeType, Node.ELEMENT_NODE);
  assert_equals(el.nodeName, "DIV");
  assert_equals(el.ownerDocument, document);
  assert_false(el.isConnected);
  document.body.appendChild(el);
  assert_true(el.isConnected);

  const text = document.createTextNode("x");
  assert_equals(text.nodeType, Node.TEXT_NODE);
  assert_equals(text.nodeName, "#text");
}, "Node.nodeType/nodeName/ownerDocument/isConnected shims");

test(() => {
  assert_equals(document.documentElement.tagName, "HTML");
  assert_equals(document.head.tagName, "HEAD");
  assert_equals(document.body.tagName, "BODY");
  assert_equals(document.documentElement.childNodes.length, 2);
  assert_equals(document.documentElement.childNodes[0].tagName, "HEAD");
  assert_equals(document.documentElement.childNodes[1].tagName, "BODY");
}, "document.documentElement/head/body shims");

test(() => {
  // Spec: if the element has no parent, `outerHTML = ...` is a no-op.
  const el = document.createElement("div");
  el.outerHTML = "<span>ignored</span>";
  assert_equals(el.outerHTML, "<div></div>");
}, "Element.outerHTML setter is a no-op on detached nodes");

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
  assert_equals(a.previousSibling, null);
  assert_equals(a.nextSibling, b);
  assert_equals(b.previousSibling, a);
  assert_equals(b.nextSibling, null);

  // Fragment insertion should be by "moving children"; appending again is a no-op.
  host.appendChild(frag);
  assert_equals(host.innerHTML, "<span>a</span><span>b</span>");
}, "Node.appendChild supports DocumentFragment insertion semantics");

test(() => {
  const container = document.createElement("div");
  const child = document.createElement("span");
  child.innerHTML = "x";
  container.appendChild(child);

  child.outerHTML = '<p id="y">y</p><p>z</p>';
  assert_equals(container.innerHTML, '<p id="y">y</p><p>z</p>');
}, "Element.outerHTML setter replaces the node in its parent");

test(() => {
  const el = document.createElement("div");
  assert_true(el instanceof HTMLElement, "HTML elements should inherit from HTMLElement");

  assert_false(el.hidden, "hidden should default to false");
  assert_equals(el.getAttribute("hidden"), null);
  el.hidden = true;
  assert_true(el.hidden);
  assert_equals(el.getAttribute("hidden"), "");
  el.hidden = false;
  assert_false(el.hidden);
  assert_equals(el.getAttribute("hidden"), null);

  assert_equals(el.title, "");
  el.title = "hello";
  assert_equals(el.getAttribute("title"), "hello");
  assert_equals(el.title, "hello");

  assert_equals(el.lang, "");
  el.lang = "en";
  assert_equals(el.getAttribute("lang"), "en");
  assert_equals(el.lang, "en");

  assert_equals(el.dir, "");
  el.dir = "ltr";
  assert_equals(el.getAttribute("dir"), "ltr");
  assert_equals(el.dir, "ltr");
}, "HTMLElement reflected attributes: hidden/title/lang/dir");

test(() => {
  const el = document.createElement("div");
  assert_true(typeof el.style === "object");
  assert_true(el.style instanceof CSSStyleDeclaration);
  assert_equals(el.style, el.style, "style should be [SameObject]");

  assert_equals(el.style.cssText, "");
  el.style.cssText = "color: red";
  assert_equals(el.getAttribute("style"), "color: red");
  assert_equals(el.style.cssText, "color: red");

  el.style.setProperty("color", "red");
  assert_true(el.getAttribute("style").includes("color: red"));
  assert_equals(el.style.getPropertyValue("color"), "red");

  el.style.width = "1px";
  assert_true(el.getAttribute("style").includes("width: 1px"));
  assert_equals(el.style.getPropertyValue("width"), "1px");

  assert_equals(el.style.removeProperty("color"), "red");
  assert_false(el.getAttribute("style").includes("color: red"));
}, "HTMLElement.style (CSSStyleDeclaration shim)");
