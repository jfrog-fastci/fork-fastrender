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

