// META: script=/resources/testharness.js

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  a.className = "x";
  root.appendChild(a);

  const list = root.querySelectorAll(".x");
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a, "NodeList.item(0)");

  const b = document.createElement("span");
  b.className = "x";
  root.appendChild(b);

  // querySelectorAll must return a static snapshot.
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a, "NodeList.item(0) after DOM mutation");

  const fresh = root.querySelectorAll(".x");
  assert_equals(typeof fresh.item, "function", "NodeList.item should be a function");
  assert_equals(fresh.length, 2);
  assert_true(list !== fresh);
}, "querySelectorAll returns a static NodeList snapshot");

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  a.className = "x";
  root.appendChild(a);

  const list = root.querySelectorAll(".x");
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a, "NodeList.item(0)");

  root.removeChild(a);

  // The snapshot must not change when nodes are removed from the tree.
  assert_equals(list.length, 1);
  assert_equals(list.item(0), a, "NodeList.item(0) after removeChild");
}, "Static NodeList returned by querySelectorAll retains removed nodes");
