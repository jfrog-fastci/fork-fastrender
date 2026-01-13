// META: script=/resources/testharness.js

test(() => {
  const root = document.createElement("div");
  const a = document.createElement("span");
  a.className = "x";
  const b = document.createElement("span");
  b.className = "x";
  const c = document.createElement("span");
  c.className = "x";
  root.appendChild(a);
  root.appendChild(b);
  root.appendChild(c);

  const list = root.querySelectorAll(".x");
  assert_equals(typeof list.forEach, "function", "NodeList.forEach should be a function");

  const thisArg = { sentinel: 42 };
  const seen = [];
  list.forEach(function(value, index, thisList) {
    assert_equals(this, thisArg, "callback this should be thisArg");
    assert_equals(thisList, list, "third argument should be the NodeList");
    seen.push([value, index]);
  }, thisArg);

  assert_equals(seen.length, 3, "forEach should visit all nodes");
  assert_equals(seen[0][0], a);
  assert_equals(seen[0][1], 0);
  assert_equals(seen[1][0], b);
  assert_equals(seen[1][1], 1);
  assert_equals(seen[2][0], c);
  assert_equals(seen[2][1], 2);
}, "NodeList.prototype.forEach iterates in order with (value, index, list) and thisArg");

test(() => {
  const root = document.createElement("div");
  root.appendChild(document.createElement("span"));
  root.appendChild(document.createElement("span"));
  const list = root.querySelectorAll("span");

  let calls = 0;
  list.forEach(function() {
    "use strict";
    assert_equals(this, undefined, "default thisArg should be undefined");
    calls++;
  });
  assert_equals(calls, 2);
}, "NodeList.prototype.forEach uses undefined as default thisArg");

test(() => {
  const root = document.createElement("div");
  root.appendChild(document.createElement("span"));
  const list = root.querySelectorAll("span");
  assert_throws_js(TypeError, () => list.forEach(null));
}, "NodeList.prototype.forEach throws TypeError for non-callable callback");

