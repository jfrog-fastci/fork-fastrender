// META: script=/resources/testharness.js
//
// Basic Range constructor + initial state checks.
//
// These align with WHATWG DOM:
// - Document.createRange() returns a new live Range.
// - new Range() exists and initializes boundary points to (document, 0).
//
// https://dom.spec.whatwg.org/#interface-range

test(() => {
  assert_equals(typeof document.createRange, "function", "document.createRange should exist");
  const range = document.createRange();
  assert_true(range instanceof Range, "document.createRange() should return a Range");

  assert_equals(range.startContainer, document, "initial startContainer should be the document");
  assert_equals(range.startOffset, 0, "initial startOffset should be 0");
  assert_equals(range.endContainer, document, "initial endContainer should be the document");
  assert_equals(range.endOffset, 0, "initial endOffset should be 0");
  assert_true(range.collapsed, "initial Range should be collapsed");
}, "Document.createRange() returns a Range with initial boundary points at (document, 0)");

test(() => {
  assert_equals(typeof Range, "function", "Range constructor should exist");
  const range = new Range();
  assert_true(range instanceof Range, "new Range() should create a Range instance");

  assert_equals(range.startContainer, document, "initial startContainer should be the document");
  assert_equals(range.startOffset, 0, "initial startOffset should be 0");
  assert_equals(range.endContainer, document, "initial endContainer should be the document");
  assert_equals(range.endOffset, 0, "initial endOffset should be 0");
  assert_true(range.collapsed, "initial Range should be collapsed");
}, "new Range() exists and initializes boundary points to (document, 0)");

