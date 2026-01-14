test(() => {
  assert_true(typeof Range === "function", "Range should be a constructor");

  const r = new Range();
  assert_true(r instanceof Range, "new Range() should create a Range instance");

  assert_equals(r.startContainer, document, "startContainer should be document");
  assert_equals(r.startOffset, 0, "startOffset should be 0");
  assert_equals(r.endContainer, document, "endContainer should be document");
  assert_equals(r.endOffset, 0, "endOffset should be 0");
  assert_true(r.collapsed, "new Range() should be collapsed");
  assert_equals(
    r.commonAncestorContainer,
    document,
    "commonAncestorContainer should be document"
  );
}, "Range constructor basics");

