// META: script=/resources/testharness.js

test(() => {
  assert_equals(document.nodeValue, null);

  const el = document.createElement("div");
  assert_equals(el.nodeValue, null);

  // Setting nodeValue on non-CharacterData nodes is a no-op.
  el.nodeValue = "x";
  assert_equals(el.nodeValue, null);
}, "Node.nodeValue is null on Document/Element and ignores writes");

test(() => {
  const text = document.createTextNode("hi");
  assert_true(text instanceof Text);
  assert_true(text instanceof Node);

  assert_equals(text.nodeValue, "hi");

  text.nodeValue = "bye";
  assert_equals(text.nodeValue, "bye");
  assert_equals(text.data, "bye");

  // Node.nodeValue is DOMString?; null/undefined act as the empty string.
  text.nodeValue = null;
  assert_equals(text.data, "");
  text.nodeValue = undefined;
  assert_equals(text.data, "");
}, "Node.nodeValue reflects Text.data and uses DOMString? coercion");

test(() => {
  const c = document.createComment("a");
  assert_true(c instanceof Comment);
  assert_true(c instanceof Node);
  assert_equals(c.nodeType, Node.COMMENT_NODE);
  assert_equals(c.nodeName, "#comment");

  assert_equals(c.data, "a");
  assert_equals(c.nodeValue, "a");

  c.data = "b";
  assert_equals(c.data, "b");
  assert_equals(c.nodeValue, "b");

  c.nodeValue = null;
  assert_equals(c.data, "");
}, "Comment.data is readable/writable and matches Node.nodeValue");

test(() => {
  const pi = document.createProcessingInstruction("target", "data");
  assert_true(pi instanceof ProcessingInstruction);
  assert_true(pi instanceof Node);

  assert_equals(pi.nodeType, Node.PROCESSING_INSTRUCTION_NODE);
  assert_equals(pi.nodeName, "target");

  assert_equals(pi.data, "data");
  assert_equals(pi.nodeValue, "data");

  pi.data = "x";
  assert_equals(pi.nodeValue, "x");

  pi.nodeValue = null;
  assert_equals(pi.data, "");
}, "ProcessingInstruction.data + Node.nodeValue");

