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

  const a = document.createElement("span");
  a.className = "foo";
  root.appendChild(a);

  assert_equals(
    typeof root.getElementsByClassName,
    "function",
    "Element.getElementsByClassName should be a function"
  );
  const list = root.getElementsByClassName("foo");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);

  a.className = "bar";
  assert_equals(list.length, 0);

  a.className = "foo bar";
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);

  const b = document.createElement("span");
  b.className = "foo";
  root.appendChild(b);
  assert_equals(list.length, 2);
  assert_equals(list.item(1), b);
}, "getElementsByClassName returns a live HTMLCollection and updates on class changes");

test(() => {
  const body = document.body;
  clear_children(body);

  const root = document.createElement("div");
  body.appendChild(root);

  const a = document.createElement("span");
  a.className = "foo bar";
  root.appendChild(a);

  assert_equals(
    typeof root.getElementsByClassName,
    "function",
    "Element.getElementsByClassName should be a function"
  );
  const list = root.getElementsByClassName("foo bar");
  assert_equals(typeof list.item, "function", "HTMLCollection.item should be a function");
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a);

  a.className = "foo";
  assert_equals(list.length, 0);
}, "getElementsByClassName with multiple classes requires all tokens to match");

test(() => {
  const body = document.body;
  clear_children(body);

  const template = document.createElement("template");
  template.innerHTML = "<span class='foo'></span>";
  body.appendChild(template);

  assert_equals(
    typeof document.getElementsByClassName,
    "function",
    "Document.getElementsByClassName should be a function"
  );
  assert_equals(document.getElementsByClassName("foo").length, 0);
  assert_equals(
    typeof template.getElementsByClassName,
    "function",
    "Element.getElementsByClassName should be a function"
  );
  assert_equals(template.getElementsByClassName("foo").length, 0);
}, "getElementsByClassName does not traverse into <template> contents");
