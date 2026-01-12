// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof document.documentURI, "string");
  assert_equals(document.documentURI, document.URL);

  assert_equals(typeof document.contentType, "string");
  assert_equals(document.contentType, "text/html");
}, "Document.documentURI + Document.contentType basics");

