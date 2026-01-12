// META: script=/resources/testharness.js
//
// Curated multi-document API checks for `Document.implementation` and
// `DOMImplementation.createHTMLDocument`.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const impl1 = document.implementation;
  const impl2 = document.implementation;

  assert_true(impl1 != null, "document.implementation should exist");
  assert_equals(impl1, impl2, "document.implementation should be stable across accesses");
}, "document.implementation exists and is stable");

test(() => {
  clear_children(document.body);

  const doc = document.implementation.createHTMLDocument("t");
  assert_true(doc instanceof Document, "createHTMLDocument should return a Document");

  // Per spec, documents created via `DOMImplementation.createHTMLDocument` are not associated with a
  // browsing context.
  assert_equals(doc.defaultView, null, "newly created HTML Documents should have defaultView === null");

  assert_equals(doc.documentElement.tagName, "HTML", "documentElement should be <html>");
  assert_equals(doc.head.tagName, "HEAD", "head should be <head>");
  assert_equals(doc.body.tagName, "BODY", "body should be <body>");

  assert_equals(doc.ownerDocument, null, "Document.ownerDocument must be null");
  assert_equals(doc.documentElement.ownerDocument, doc, "<html> ownerDocument should be the created document");
  assert_equals(doc.head.ownerDocument, doc, "<head> ownerDocument should be the created document");
  assert_equals(doc.body.ownerDocument, doc, "<body> ownerDocument should be the created document");
}, "DOMImplementation.createHTMLDocument returns a detached HTML Document with correct ownerDocument links");
