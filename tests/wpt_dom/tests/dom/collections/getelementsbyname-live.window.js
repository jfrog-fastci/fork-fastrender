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

  assert_equals(
    typeof document.getElementsByName,
    "function",
    "Document.getElementsByName should be a function"
  );
  const list = document.getElementsByName("x");
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.length, 0);

  const a = document.createElement("div");
  a.setAttribute("name", "x");
  root.appendChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);

  a.setAttribute("name", "y");
  assert_equals(list.length, 0);

  const b = document.createElement("div");
  b.setAttribute("name", "x");
  root.appendChild(b);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), b);

  root.removeChild(b);
  assert_equals(list.length, 0);
}, "Document.getElementsByName returns a live NodeList");

test(() => {
  const body = document.body;
  clear_children(body);

  const template = document.createElement("template");
  template.innerHTML = "<div name='x'></div>";
  body.appendChild(template);

  assert_equals(
    typeof document.getElementsByName,
    "function",
    "Document.getElementsByName should be a function"
  );
  assert_equals(document.getElementsByName("x").length, 0);
}, "getElementsByName does not traverse into <template> contents");
