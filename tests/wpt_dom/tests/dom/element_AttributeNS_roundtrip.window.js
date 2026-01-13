// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");

  el.setAttributeNS("http://example.com/ns", "p:foo", "1");
  assert_equals(
    el.getAttributeNS("http://example.com/ns", "foo"),
    "1",
    "getAttributeNS must match by (namespace, localName)"
  );

  el.removeAttributeNS("http://example.com/ns", "foo");
  assert_equals(
    el.getAttributeNS("http://example.com/ns", "foo"),
    null,
    "removeAttributeNS must remove the namespaced attribute"
  );
}, "Element AttributeNS roundtrip for non-null namespace");

