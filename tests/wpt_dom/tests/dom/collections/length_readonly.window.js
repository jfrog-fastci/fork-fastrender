// META: script=/resources/testharness.js

test(() => {
  "use strict";

  const node = document.createElement("div");
  const l = node.childNodes;

  assert_throws_js(TypeError, () => {
    l.length = 5;
  });

  assert_equals(l.length, 0);

  const a = document.createElement("span");
  node.appendChild(a);
  assert_equals(l.length, 1);

  node.removeChild(a);
  assert_equals(l.length, 0);
}, "NodeList.length is readonly and stays live");

