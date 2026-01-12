// META: script=/resources/testharness.js

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  body.appendChild(root);

  assert_equals(typeof root.getElementsByTagName, "function", "Element.getElementsByTagName should be a function");
  const list = root.getElementsByTagName("span");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 0);

  const a = document.createElement("span");
  root.appendChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a, "item(0) after appendChild");

  const b = document.createElement("span");
  root.appendChild(b);
  assert_equals(list.length, 2);
  assert_equals(list.item(1), b, "item(1) after second appendChild");

  root.removeChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), b, "item(0) after removeChild");
}, "getElementsByTagName returns a live HTMLCollection");

test(() => {
  const body = document.body;
  clear_children(body);

  const template = document.createElement("template");
  template.innerHTML = "<span class='x'></span>";
  body.appendChild(template);

  // Elements inside template contents must not be reachable through tree-traversal collection APIs.
  assert_equals(typeof document.getElementsByTagName, "function", "Document.getElementsByTagName should be a function");
  assert_equals(document.getElementsByTagName("span").length, 0);
  assert_equals(typeof template.getElementsByTagName, "function", "Element.getElementsByTagName should be a function");
  assert_equals(template.getElementsByTagName("span").length, 0);
}, "getElementsByTagName does not traverse into <template> contents");
