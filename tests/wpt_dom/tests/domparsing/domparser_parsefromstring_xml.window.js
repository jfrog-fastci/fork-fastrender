// META: script=/resources/testharness.js

test(() => {
  const doc = new DOMParser().parseFromString(
    '<root xmlns="urn:x"><child a="1">text</child></root>',
    "application/xml"
  );

  assert_true(doc instanceof Document);
  assert_equals(doc.contentType, "application/xml");
  assert_equals(doc.documentElement.nodeName, "root");

  const child = doc.querySelector("child");
  assert_true(child instanceof Element);
  assert_equals(child.getAttribute("a"), "1");
}, "DOMParser.parseFromString(application/xml) parses well-formed XML");

