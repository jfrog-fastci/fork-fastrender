// META: script=/resources/testharness.js

test(() => {
  const doc = new DOMParser().parseFromString(
    '<root xmlns="urn:x"><child a="1">text</child><!--c--></root>',
    "application/xml"
  );

  const s = new XMLSerializer().serializeToString(doc.documentElement);

  assert_true(s.includes("<root"), "should contain the root element");
  assert_true(s.includes('xmlns="urn:x"'), "should contain the namespace declaration");
  assert_true(s.includes('<child a="1">text</child>'), "should contain the child element");
  assert_true(s.includes("<!--c-->"), "should contain the comment");
}, "XMLSerializer.serializeToString serializes an XML element subtree");

