// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);

  const list = parent.children;
  assert_equals(typeof list.values, "function", "HTMLCollection.values should be a function");
  assert_equals(list[Symbol.iterator], list.values, "@@iterator should alias values()");
  assert_equals(
    HTMLCollection.prototype[Symbol.iterator],
    HTMLCollection.prototype.values
  );

  assert_array_equals(Array.from(list.values()), [a, b], "Array.from(values())");
  assert_array_equals(Array.from(list.keys()), [0, 1], "Array.from(keys())");

  const entries = Array.from(list.entries());
  assert_equals(entries.length, 2);
  assert_array_equals(entries[0], [0, a]);
  assert_array_equals(entries[1], [1, b]);

  const thisArg = { tag: "x" };
  const visited = [];
  list.forEach(function (node, i, collection) {
    assert_equals(this, thisArg, "thisArg should be passed through");
    visited.push([node, i, collection]);
  }, thisArg);

  assert_equals(visited.length, 2);
  assert_equals(visited[0][0], a);
  assert_equals(visited[0][1], 0);
  assert_equals(visited[0][2], list);
  assert_equals(visited[1][0], b);
  assert_equals(visited[1][1], 1);
  assert_equals(visited[1][2], list);
}, "HTMLCollection iterable helpers: values/keys/entries/forEach (children)");

