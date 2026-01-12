// META: script=/resources/testharness.js

test(() => {
  const doc = new DOMParser().parseFromString("<root>", "application/xml");

  assert_true(doc instanceof Document);
  assert_equals(doc.contentType, "application/xml");
  assert_equals(doc.documentElement.nodeName, "parsererror");

  if (typeof XMLSerializer === "function") {
    const s = new XMLSerializer().serializeToString(doc.documentElement);
    assert_true(s.includes("parsererror"));
  }
}, "DOMParser.parseFromString(application/xml) returns a parsererror document for invalid XML");

