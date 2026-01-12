// META: script=/resources/testharness.js

test(() => {
  const dt = document.implementation.createDocumentType("html", "pub", "sys");
  assert_equals(dt.nodeType, 10, "DocumentType nodeType must be 10");
  assert_equals(dt.name, "html");
  assert_equals(dt.publicId, "pub");
  assert_equals(dt.systemId, "sys");
}, "DOMImplementation.createDocumentType creates a DocumentType node with the requested identifiers");

