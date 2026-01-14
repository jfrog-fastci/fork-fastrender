// META: script=/resources/testharness.js

test(() => {
  const doc = document.implementation.createDocument(null, "root", null);
  assert_not_equals(doc, window.document, "createDocument should return a new Document");
  assert_equals(doc.nodeType, 9, "nodeType should be DOCUMENT_NODE (9)");
  assert_not_equals(doc.documentElement, null, "documentElement should be non-null");
  assert_equals(doc.documentElement.tagName, "root");
}, "DOMImplementation.createDocument creates an XML document with a documentElement when qualifiedName is non-empty");

