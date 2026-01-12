// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "a<!--c-->b";

  assert_equals(el.innerHTML, "a<!--c-->b");
  assert_equals(el.outerHTML, "<div>a<!--c-->b</div>");
}, "Element.innerHTML/outerHTML round-trips HTML comments");

test(() => {
  const el = document.createElement("div");
  el.innerHTML = "a<!--c-->b";

  assert_equals(Node.COMMENT_NODE, 8, "Node.COMMENT_NODE should be 8");

  assert_equals(el.childNodes.length, 3, "expected 3 children: Text, Comment, Text");
  if (el.childNodes.length !== 3) return;

  const text_a = el.childNodes[0];
  const comment = el.childNodes[1];
  const text_b = el.childNodes[2];

  assert_equals(text_a.nodeType, Node.TEXT_NODE, "first child should be a Text node");
  assert_equals(comment.nodeType, Node.COMMENT_NODE, "second child should be a Comment node");
  assert_equals(text_b.nodeType, Node.TEXT_NODE, "third child should be a Text node");

  assert_equals(text_a.data, "a");
  assert_equals(comment.nodeName, "#comment");
  assert_equals(comment.data, "c");
  assert_equals(text_b.data, "b");

  assert_equals(text_a.nextSibling, comment);
  assert_equals(comment.previousSibling, text_a);
  assert_equals(comment.nextSibling, text_b);
  assert_equals(text_b.previousSibling, comment);
}, "Comment nodes appear in childNodes and affect sibling relationships");
