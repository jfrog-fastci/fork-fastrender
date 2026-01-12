// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "<!--x--><span>hi</span>";
  assert_equals(el.innerHTML, "<!--x--><span>hi</span>");

  assert_equals(el.childNodes.length, 2);
  assert_equals(el.firstChild.nodeType, Node.COMMENT_NODE);
  assert_equals(el.firstChild.nodeName, "#comment");
}, "Element.innerHTML preserves comments");

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "<!doctype html><!--x--><span>hi</span>";
  assert_equals(el.innerHTML, "<!--x--><span>hi</span>");
}, "Element.innerHTML ignores doctype tokens in fragments but preserves comments");

test(() => {
  const parent = document.createElement("div");
  const child = document.createElement("span");
  parent.appendChild(child);

  child.outerHTML = "<!--c--><p>two</p>";
  assert_equals(parent.innerHTML, "<!--c--><p>two</p>");
}, "Element.outerHTML setter preserves comment nodes in parsed fragments");

