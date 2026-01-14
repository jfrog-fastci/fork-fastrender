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
  el.setAttribute("ID", "a");

  assert_equals(typeof el.getAttributeNodeNS, "function", "getAttributeNodeNS should exist");

  const attr = el.getAttributeNodeNS(null, "id");
  assert_true(attr instanceof Attr, "getAttributeNodeNS should return an Attr");
  assert_equals(attr.ownerElement, el, "Attr.ownerElement should be the owning element");
  assert_equals(attr.value, "a", "Attr.value should reflect the attribute value");
  assert_equals(attr.name, "id", "Attr.name should be the qualified name (lowercased in HTML)");
  assert_equals(attr.localName, "id", "Attr.localName should be the local name");
  assert_equals(attr.prefix, null, "null-namespace Attr.prefix should be null");
  assert_equals(attr.namespaceURI, null, "null-namespace Attr.namespaceURI should be null");

  assert_equals(el.getAttributeNodeNS(null, "missing"), null, "missing attribute should return null");
}, "Element.getAttributeNodeNS returns a branded Attr for null namespace attributes");

test(() => {
  const el = document.createElement("div");
  el.setAttributeNS("http://example.com/ns", "p:foo", "1");

  assert_equals(typeof el.getAttributeNodeNS, "function", "getAttributeNodeNS should exist");
  const attr = el.getAttributeNodeNS("http://example.com/ns", "foo");
  assert_true(attr instanceof Attr, "getAttributeNodeNS should return an Attr");
  assert_equals(attr.ownerElement, el, "Attr.ownerElement should be the owning element");
  assert_equals(attr.value, "1", "Attr.value should reflect the attribute value");
  assert_equals(attr.name, "p:foo", "Attr.name should be the qualified name");
  assert_equals(attr.localName, "foo", "Attr.localName should be the local name");
  assert_equals(attr.prefix, "p", "Attr.prefix should reflect the attribute prefix");
  assert_equals(attr.namespaceURI, "http://example.com/ns", "Attr.namespaceURI should reflect the attribute namespace");

  assert_equals(typeof el.attributes.getNamedItemNS, "function", "NamedNodeMap.getNamedItemNS should exist");
  const viaMap = el.attributes.getNamedItemNS("http://example.com/ns", "foo");
  assert_true(viaMap instanceof Attr, "getNamedItemNS should return an Attr");
  assert_equals(viaMap.name, "p:foo");
  assert_equals(viaMap.localName, "foo");
  assert_equals(viaMap.prefix, "p");
  assert_equals(viaMap.namespaceURI, "http://example.com/ns");
  assert_equals(viaMap.value, "1");

  assert_equals(el.getAttributeNodeNS("http://example.com/other", "foo"), null, "wrong namespace should return null");
  assert_equals(el.getAttributeNodeNS("http://example.com/ns", "missing"), null, "missing attribute should return null");
}, "Element.getAttributeNodeNS/NamedNodeMap.getNamedItemNS match by (namespace, localName)");
