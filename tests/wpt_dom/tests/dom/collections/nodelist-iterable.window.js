// META: script=/resources/testharness.js

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);

  const out = [];
  for (const node of root.childNodes) {
    out.push(node);
  }
  assert_equals(out.length, 2);
  assert_equals(out[0], a);
  assert_equals(out[1], b);
}, "NodeList (childNodes) is iterable via for..of");

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  root.appendChild(a);
  root.appendChild(b);

  const list = root.childNodes;
  assert_equals(typeof list.values, "function", "NodeList.values should be a function");
  assert_equals(list[Symbol.iterator], list.values, "@@iterator should alias values()");
  assert_equals(NodeList.prototype[Symbol.iterator], NodeList.prototype.values);
  assert_equals(typeof NodeList.prototype.values, "function");
  assert_equals(typeof NodeList.prototype.keys, "function");
  assert_equals(typeof NodeList.prototype.entries, "function");

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
}, "NodeList iterable helpers: values/keys/entries/forEach (childNodes)");

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  a.className = "x";
  const b = document.createElement("span");
  b.className = "x";
  root.appendChild(a);
  root.appendChild(b);

  const out = [];
  for (const node of root.querySelectorAll(".x")) {
    out.push(node);
  }
  assert_equals(out.length, 2);
  assert_equals(out[0], a);
  assert_equals(out[1], b);
}, "NodeList (querySelectorAll) is iterable via for..of");

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  a.className = "x";
  const b = document.createElement("span");
  b.className = "x";
  root.appendChild(a);
  root.appendChild(b);

  const list = root.querySelectorAll(".x");
  assert_equals(typeof list.values, "function", "NodeList.values should be a function");
  assert_equals(list[Symbol.iterator], list.values, "@@iterator should alias values()");

  assert_array_equals(Array.from(list.values()), [a, b], "Array.from(values())");
  assert_array_equals(Array.from(list.keys()), [0, 1], "Array.from(keys())");
  const entries = Array.from(list.entries());
  assert_equals(entries.length, 2);
  assert_array_equals(entries[0], [0, a]);
  assert_array_equals(entries[1], [1, b]);

  const visited = [];
  list.forEach((node) => visited.push(node));
  assert_array_equals(visited, [a, b], "forEach() visits nodes in order");
}, "NodeList iterable helpers work on static NodeLists (querySelectorAll)");

test(() => {
  // Ensure a stable, querySelectorAll-produced NodeList with a known length.
  const host = document.createElement("div");
  host.id = "nodelist-iterable-test";
  document.body.appendChild(host);
  host.innerHTML = "<div></div><div></div><div></div>";

  const list = document.querySelectorAll("#nodelist-iterable-test > div");
  assert_false(Array.isArray(list), "querySelectorAll must not return an Array");
  assert_equals(Object.getPrototypeOf(list), NodeList.prototype, "NodeList prototype");
  assert_true(list instanceof NodeList, "NodeList instanceof check");
  assert_array_equals(Array.from(list.keys()), [0, 1, 2]);

  const entries = Array.from(list.entries());
  assert_equals(entries[0][0], 0);
  assert_equals(entries[0][1], list[0]);

  document.body.removeChild(host);
}, "NodeList keys() yields indices and entries() yields [index, value] pairs");

test(() => {
  assert_equals(typeof HTMLCollection.prototype.values, "function");
  assert_equals(HTMLCollection.prototype.values, HTMLCollection.prototype[Symbol.iterator]);
  assert_equals(typeof HTMLCollection.prototype.keys, "function");
  assert_equals(typeof HTMLCollection.prototype.entries, "function");
}, "HTMLCollection iterable methods (values/keys/entries) are installed and values aliases @@iterator");

test(() => {
  const host = document.createElement("div");
  host.id = "htmlcollection-iterable-test";
  document.body.appendChild(host);
  host.innerHTML = "<div></div><div></div><div></div>";

  const coll = host.children;
  assert_array_equals(Array.from(coll.keys()), [0, 1, 2]);

  const entries = Array.from(coll.entries());
  assert_equals(entries[0][0], 0);
  assert_equals(entries[0][1], coll[0]);

  document.body.removeChild(host);
}, "HTMLCollection keys() yields indices and entries() yields [index, value] pairs");
