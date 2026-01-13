// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  el.setAttribute("id", "a");

  assert_equals(typeof el.getAttributeNode, "function", "getAttributeNode should exist");

  const attr = el.getAttributeNode("id");
  assert_true(attr instanceof Attr, "getAttributeNode should return an Attr");
  assert_equals(attr.ownerElement, el, "Attr.ownerElement should be the owning element");
  assert_equals(attr.value, "a", "Attr.value should reflect the attribute value");

  assert_equals(el.getAttributeNode("missing"), null, "missing attribute should return null");
}, "Element.getAttributeNode returns a branded Attr object");

test(() => {
  const el = document.createElement("div");
  el.setAttribute("id", "a");

  assert_equals(typeof el.getAttributeNodeNS, "function", "getAttributeNodeNS should exist");

  const attr = el.getAttributeNodeNS(null, "id");
  assert_true(attr instanceof Attr, "getAttributeNodeNS should return an Attr");
  assert_equals(attr.ownerElement, el, "Attr.ownerElement should be the owning element");
  assert_equals(attr.value, "a", "Attr.value should reflect the attribute value");

  assert_equals(el.getAttributeNodeNS(null, "missing"), null, "missing attribute should return null");
}, "Element.getAttributeNodeNS exists and ignores namespace");

