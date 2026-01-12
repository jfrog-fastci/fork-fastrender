// META: script=/resources/testharness.js

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

const HTML_NS = "http://www.w3.org/1999/xhtml";

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  body.appendChild(root);

  const a = document.createElement("span");
  root.appendChild(a);

  assert_equals(
    typeof root.getElementsByTagNameNS,
    "function",
    "Element.getElementsByTagNameNS should be a function"
  );
  const list = root.getElementsByTagNameNS(HTML_NS, "SPAN");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);
}, "getElementsByTagNameNS matches elements in the HTML namespace case-insensitively");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  body.appendChild(root);

  assert_equals(
    typeof root.getElementsByTagNameNS,
    "function",
    "Element.getElementsByTagNameNS should be a function"
  );
  const list = root.getElementsByTagNameNS(HTML_NS, "span");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 0);

  const a = document.createElement("span");
  root.appendChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);
}, "getElementsByTagNameNS returns a live HTMLCollection");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  body.appendChild(root);

  const a = document.createElement("span");
  root.appendChild(a);

  // Namespace is a nullable argument in WebIDL; null means "no namespace".
  assert_equals(
    typeof root.getElementsByTagNameNS,
    "function",
    "Element.getElementsByTagNameNS should be a function"
  );
  const list = root.getElementsByTagNameNS(null, "span");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 0);
}, "getElementsByTagNameNS(null, localName) only matches elements with a null namespace");

test(() => {
  const body = document.body;
  clear_children(body);

  const template = document.createElement("template");
  template.innerHTML = "<span></span>";
  body.appendChild(template);

  assert_equals(
    typeof document.getElementsByTagNameNS,
    "function",
    "Document.getElementsByTagNameNS should be a function"
  );
  assert_equals(document.getElementsByTagNameNS(HTML_NS, "span").length, 0);
  assert_equals(
    typeof template.getElementsByTagNameNS,
    "function",
    "Element.getElementsByTagNameNS should be a function"
  );
  assert_equals(template.getElementsByTagNameNS(HTML_NS, "span").length, 0);
}, "getElementsByTagNameNS does not traverse into <template> contents");
