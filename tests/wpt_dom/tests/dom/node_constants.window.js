// META: script=/resources/testharness.js

test(() => {
  assert_equals(Node.ATTRIBUTE_NODE, 2);
  assert_equals(Node.CDATA_SECTION_NODE, 4);
  assert_equals(Node.PROCESSING_INSTRUCTION_NODE, 7);
  assert_equals(Node.COMMENT_NODE, 8);
  assert_equals(Node.DOCUMENT_TYPE_NODE, 10);
}, "Node constructor exposes required nodeType constants");
