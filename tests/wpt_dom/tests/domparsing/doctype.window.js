// META: script=/resources/testharness.js

test(() => {
  assert_true(document.doctype !== null, "expected a doctype node");
  assert_true(document.doctype instanceof DocumentType);

  assert_equals(document.doctype.nodeType, Node.DOCUMENT_TYPE_NODE);
  assert_equals(document.doctype.name, "html");
  assert_equals(document.doctype.publicId, "");
  assert_equals(document.doctype.systemId, "");

  assert_equals(document.childNodes[0], document.doctype);
}, "document.doctype returns the parsed DocumentType node");
