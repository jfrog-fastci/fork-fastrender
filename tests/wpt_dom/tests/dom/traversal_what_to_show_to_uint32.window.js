// META: script=/resources/testharness.js
//
// WebIDL conversion semantics for the `whatToShow` argument used by the DOM Traversal APIs.
//
// Spec:
// - https://dom.spec.whatwg.org/#dom-document-createnodeiterator
// - https://dom.spec.whatwg.org/#dom-document-createtreewalker
//
// `whatToShow` is an `unsigned long`, so WebIDL conversion must use `ToUint32` (mod 2^32)
// semantics rather than a saturating cast (e.g. -1 => 2^32-1, not 0).

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, -1, null);
  assert_equals(it.whatToShow, NodeFilter.SHOW_ALL);
}, "Document.createNodeIterator: whatToShow uses ToUint32 (-1 => SHOW_ALL)");

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, NaN, null);
  assert_equals(it.whatToShow, 0);
}, "Document.createNodeIterator: whatToShow uses ToUint32 (NaN => 0)");

test(() => {
  const root = document.createElement("div");
  const it = document.createNodeIterator(root, 4294967297, null); // 2^32 + 1
  assert_equals(it.whatToShow, NodeFilter.SHOW_ELEMENT);
}, "Document.createNodeIterator: whatToShow uses ToUint32 (2^32+1 => 1)");

test(() => {
  const root = document.createElement("div");
  const tw = document.createTreeWalker(root, -1, null);
  assert_equals(tw.whatToShow, NodeFilter.SHOW_ALL);
}, "Document.createTreeWalker: whatToShow uses ToUint32 (-1 => SHOW_ALL)");

test(() => {
  const root = document.createElement("div");
  const tw = document.createTreeWalker(root, NaN, null);
  assert_equals(tw.whatToShow, 0);
}, "Document.createTreeWalker: whatToShow uses ToUint32 (NaN => 0)");

test(() => {
  const root = document.createElement("div");
  const tw = document.createTreeWalker(root, 4294967297, null); // 2^32 + 1
  assert_equals(tw.whatToShow, NodeFilter.SHOW_ELEMENT);
}, "Document.createTreeWalker: whatToShow uses ToUint32 (2^32+1 => 1)");

