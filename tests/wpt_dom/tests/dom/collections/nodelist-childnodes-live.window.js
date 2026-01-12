// META: script=/resources/testharness.js

test(() => {
  const node = document.createElement("div");
  const a = node.childNodes;
  const b = node.childNodes;
  assert_equals(a, b);
}, "Node.childNodes is [SameObject]");

test(() => {
  const node = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createTextNode("hello");
  const c = document.createComment("c");
  node.appendChild(a);
  node.appendChild(b);
  node.appendChild(c);

  const list = node.childNodes;
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.length, 3);
  assert_equals(list.item(0), a, "NodeList.item(0)");
  assert_equals(list.item(1), b, "NodeList.item(1)");
  assert_equals(list.item(2), c, "NodeList.item(2)");
  assert_equals(list.item(3), null, "NodeList.item(out of range) is null");

  // NodeList has indexed property access via its WebIDL getter.
  assert_equals(list[0], a, "NodeList[0]");
  assert_equals(list[1], b, "NodeList[1]");
  assert_equals(list[2], c, "NodeList[2]");
}, "Node.childNodes returns a NodeList of all child nodes");

test(() => {
  const node = document.createElement("div");
  const list = node.childNodes;

  assert_equals(list.length, 0);

  const a = document.createElement("span");
  node.appendChild(a);
  assert_equals(list.length, 1);
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.item(0), a, "NodeList.item(0) after appendChild");

  const b = document.createElement("span");
  node.appendChild(b);
  assert_equals(list.length, 2);
  assert_equals(list.item(1), b, "NodeList.item(1) after second appendChild");

  node.removeChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), b, "NodeList.item(0) after removeChild");
}, "Node.childNodes NodeList updates live on DOM mutation");
