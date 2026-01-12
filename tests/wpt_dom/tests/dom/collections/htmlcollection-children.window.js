// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");
  const a = parent.children;
  const b = parent.children;
  assert_equals(a, b);
}, "Element.children is [SameObject]");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const text = document.createTextNode("t");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(text);
  parent.appendChild(b);

  assert_true(parent.children !== null && parent.children !== undefined, "Element.children should exist");
  assert_equals(typeof parent.children.item, "function", "HTMLCollection.item should be a function");

  assert_equals(parent.childElementCount, 2, "childElementCount");
  assert_equals(parent.children.length, 2, "children.length");
  assert_equals(parent.children.item(0), a, "children.item(0)");
  assert_equals(parent.children.item(1), b, "children.item(1)");

  // HTMLCollection has indexed property access via its WebIDL getter.
  assert_equals(parent.children[0], a, "children[0]");
  assert_equals(parent.children[1], b, "children[1]");

  assert_equals(parent.firstElementChild, a, "firstElementChild");
  assert_equals(parent.lastElementChild, b, "lastElementChild");
  assert_equals(a.nextElementSibling, b, "nextElementSibling");
  assert_equals(b.previousElementSibling, a, "previousElementSibling");
}, "Element.children and element-sibling traversal ignore non-element nodes");

test(() => {
  const parent = document.createElement("div");
  const children = parent.children;

  assert_true(children !== null && children !== undefined, "Element.children should exist");
  assert_equals(typeof children.item, "function", "HTMLCollection.item should be a function");

  assert_equals(children.length, 0);
  assert_equals(parent.firstElementChild, null);
  assert_equals(parent.lastElementChild, null);

  const a = document.createElement("span");
  parent.appendChild(a);
  assert_equals(children.length, 1);
  assert_equals(children.item(0), a, "children.item(0) after appendChild");
  assert_equals(parent.firstElementChild, a);
  assert_equals(parent.lastElementChild, a);

  const b = document.createElement("span");
  parent.appendChild(b);
  assert_equals(children.length, 2);
  assert_equals(children.item(1), b, "children.item(1) after second appendChild");
  assert_equals(parent.firstElementChild, a);
  assert_equals(parent.lastElementChild, b);

  parent.removeChild(a);
  assert_equals(children.length, 1);
  assert_equals(children.item(0), b, "children.item(0) after removeChild");
  assert_equals(parent.firstElementChild, b);
  assert_equals(parent.lastElementChild, b);
}, "Element.children HTMLCollection updates live on DOM mutation");
