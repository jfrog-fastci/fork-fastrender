// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "<span>hi</span>";
  assert_equals(el.innerHTML, "<span>hi</span>");
  assert_equals(el.outerHTML, "<div><span>hi</span></div>");
}, "Element.innerHTML/outerHTML basic set/get");

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "a & b";
  assert_equals(el.innerHTML, "a &amp; b");
}, "Element.innerHTML escapes '&' on serialization");

test(() => {
  const parent = document.createElement("div");
  const child = document.createElement("span");
  parent.appendChild(child);
  child.outerHTML = "<p>one</p><p>two</p>";
  assert_equals(parent.innerHTML, "<p>one</p><p>two</p>");
}, "Element.outerHTML setter replaces node with a parsed fragment");
