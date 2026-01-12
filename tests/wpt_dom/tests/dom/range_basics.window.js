// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof document.createRange, "function");
  assert_equals(typeof Range, "function");
  assert_equals(typeof AbstractRange, "function");
}, "Range/AbstractRange baseline APIs exist");

test(() => {
  const r = document.createRange();

  assert_true(r instanceof Range, "range is a Range");
  assert_true(r instanceof AbstractRange, "range is an AbstractRange");

  assert_equals(r.startContainer, document);
  assert_equals(r.endContainer, document);
  assert_equals(r.startOffset, 0);
  assert_equals(r.endOffset, 0);
  assert_true(r.collapsed);
  assert_equals(r.commonAncestorContainer, document);
}, "Document.createRange() returns a collapsed Range at (document, 0)");

test(() => {
  const r = new Range();

  assert_true(r instanceof Range, "range is a Range");
  assert_true(r instanceof AbstractRange, "range is an AbstractRange");

  assert_equals(r.startContainer, document);
  assert_equals(r.endContainer, document);
  assert_equals(r.startOffset, 0);
  assert_equals(r.endOffset, 0);
  assert_true(r.collapsed);
  assert_equals(r.commonAncestorContainer, document);
}, "new Range() returns a collapsed Range at (document, 0)");

test(() => {
  const r = document.createRange();

  r.setStart(document.body, 0);

  assert_equals(r.startContainer, document.body);
  assert_equals(r.startOffset, 0);

  // When the new start boundary point is after the old end boundary point, the
  // setStart() algorithm collapses the range to the new start.
  assert_equals(r.endContainer, document.body);
  assert_equals(r.endOffset, 0);
  assert_true(r.collapsed);
}, "Range.setStart() updates the start boundary and collapses when start moves after end");

test(() => {
  const r = document.createRange();
  assert_equals(typeof r.createContextualFragment, "function");
}, "Range.prototype.createContextualFragment exists");
