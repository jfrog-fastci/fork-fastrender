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

  assert_equals(comment.nodeValue, "c");
  assert_equals(comment.textContent, "c");
  assert_equals(el.textContent, "ab", "Element.textContent should ignore comments");

  assert_equals(text_a.nextSibling, comment);
  assert_equals(comment.previousSibling, text_a);
  assert_equals(comment.nextSibling, text_b);
  assert_equals(text_b.previousSibling, comment);

  comment.data = "d";
  assert_equals(el.innerHTML, "a<!--d-->b");
  assert_equals(el.textContent, "ab");
}, "Comment nodes appear in childNodes and affect sibling relationships");

test(() => {
  const el = document.createElement("div");
  const comment = document.createComment("c");
  el.appendChild(document.createTextNode("a"));
  el.appendChild(comment);
  el.appendChild(document.createTextNode("b"));

  assert_equals(comment.nodeType, Node.COMMENT_NODE, "createComment() should create a Comment node");
  assert_equals(comment.data, "c");
  assert_equals(comment.nodeValue, "c");
  assert_equals(comment.textContent, "c");
  assert_equals(comment.ownerDocument, document);

  assert_equals(el.innerHTML, "a<!--c-->b", "Comment node should serialize in innerHTML");

  comment.textContent = "d";
  assert_equals(comment.data, "d", "Setting Comment.textContent should update data");
  assert_equals(el.innerHTML, "a<!--d-->b");

  el.removeChild(comment);
  assert_equals(el.innerHTML, "ab", "Removing a Comment should remove its serialization");
  assert_equals(el.childNodes.length, 2, "Removing a Comment should leave the two Text siblings");
  assert_equals(el.childNodes[0].data, "a");
  assert_equals(el.childNodes[1].data, "b");
  assert_equals(el.childNodes[0].nextSibling, el.childNodes[1]);
  assert_equals(el.childNodes[1].previousSibling, el.childNodes[0]);
}, "document.createComment() creates Comment nodes that serialize and participate in tree mutation");
