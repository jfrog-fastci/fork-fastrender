// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof document.compatMode, "string");
  assert_equals(document.compatMode, "CSS1Compat");
}, "Document.compatMode defaults to CSS1Compat in no-quirks documents");

