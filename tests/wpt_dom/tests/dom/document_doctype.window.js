// META: script=/resources/testharness.js

test(() => {
  assert_true("doctype" in document, "Document should expose a doctype property");

  const doctype = document.doctype;
  assert_true(
    doctype !== null && doctype !== undefined,
    "document.doctype should be non-null for HTML documents"
  );
  if (doctype === null || doctype === undefined) return;

  assert_equals(Node.DOCUMENT_TYPE_NODE, 10, "Node.DOCUMENT_TYPE_NODE should be 10");
  assert_equals(doctype.nodeType, Node.DOCUMENT_TYPE_NODE);

  assert_equals(doctype.name, "html");
  assert_equals(doctype.publicId, "");
  assert_equals(doctype.systemId, "");
}, "document.doctype exposes the HTML DocumentType node and default identifiers");

test(() => {
  const doctype = document.doctype;
  assert_true(doctype !== null && doctype !== undefined, "expected a doctype node");
  if (doctype === null || doctype === undefined) return;

  // HTML documents have the doctype before the documentElement.
  assert_equals(document.childNodes[0], doctype);
  assert_equals(document.childNodes[1], document.documentElement);

  assert_equals(doctype.nextSibling, document.documentElement);
  assert_equals(document.documentElement.previousSibling, doctype);
}, "DocumentType participates in document.childNodes ordering");
