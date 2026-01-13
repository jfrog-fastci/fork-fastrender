// META: script=/resources/testharness.js
//
// WebIDL conversion semantics for `Document.createNodeIterator`'s `whatToShow` argument.
//
// Spec: https://dom.spec.whatwg.org/#dom-document-createnodeiterator

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, -1, null);
  assert_equals(it.whatToShow, NodeFilter.SHOW_ALL);
}, "Document.createNodeIterator: whatToShow is converted using ToUint32 (-1 => SHOW_ALL)");

