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

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, NaN, null);
  assert_equals(it.whatToShow, 0);
}, "Document.createNodeIterator: whatToShow is converted using ToUint32 (NaN => 0)");

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, 4294967297, null); // 2^32 + 1
  assert_equals(it.whatToShow, NodeFilter.SHOW_ELEMENT);
}, "Document.createNodeIterator: whatToShow is converted using ToUint32 (2^32+1 => 1)");
