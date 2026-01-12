// META: script=/resources/testharness.js
//
// Curated namespace + qualified-name tests for `Document.createElementNS()` and Element naming
// properties (`namespaceURI`, `localName`, `prefix`).

function assert_throws_dom_name(expected, fn, message) {
  let caught = null;
  try {
    fn();
  } catch (e) {
    caught = e;
  }
  assert_true(caught !== null, message || "expected function to throw");
  assert_equals(caught.name, expected, message || "unexpected exception name");
}

test(() => {
  const el = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  assert_equals(el.namespaceURI, "http://www.w3.org/2000/svg");
  assert_equals(el.localName, "svg");
  assert_equals(el.prefix, null);
  assert_equals(el.tagName, "svg");
}, "Document.createElementNS creates SVG elements with correct naming properties");

test(() => {
  const el = document.createElementNS("http://www.w3.org/1999/xhtml", "DiV");
  assert_equals(el.namespaceURI, "http://www.w3.org/1999/xhtml");
  assert_equals(el.localName, "div", "HTML namespace localName must be ASCII-lowercased");
  assert_equals(el.prefix, null);
  assert_equals(el.tagName, "DIV", "HTML namespace tagName must be ASCII-uppercased");
}, "Document.createElementNS HTML namespace lowercases localName and uppercases tagName");

test(() => {
  const el = document.createElementNS("http://www.w3.org/2000/svg", "svg:rect");
  assert_equals(el.namespaceURI, "http://www.w3.org/2000/svg");
  assert_equals(el.localName, "rect");
  assert_equals(el.prefix, "svg");
  assert_equals(el.tagName, "svg:rect");
}, "Document.createElementNS preserves prefix and localName");

test(() => {
  const el = document.createElementNS(null, "DiV");
  assert_equals(el.namespaceURI, null);
  assert_equals(el.localName, "DiV");
  assert_equals(el.prefix, null);
  assert_equals(el.tagName, "DiV");
}, "Document.createElementNS with null namespace creates no-namespace elements");

test(() => {
  assert_throws_dom_name("InvalidCharacterError", () => {
    document.createElementNS("http://www.w3.org/2000/svg", "svg::rect");
  }, "multiple colons in qualifiedName should be InvalidCharacterError");
  assert_throws_dom_name("InvalidCharacterError", () => {
    document.createElementNS("http://www.w3.org/2000/svg", "1abc");
  }, "names starting with digits should be InvalidCharacterError");
}, "Document.createElementNS rejects invalid qualified names with InvalidCharacterError");

test(() => {
  assert_throws_dom_name("NamespaceError", () => {
    document.createElementNS("http://www.w3.org/2000/svg", "xmlns:test");
  }, "xmlns prefix requires XMLNS namespace");
  assert_throws_dom_name("NamespaceError", () => {
    document.createElementNS("http://www.w3.org/2000/xmlns/", "foo");
  }, "XMLNS namespace requires xmlns qualifiedName");
}, "Document.createElementNS enforces XMLNS namespace rules");

test(() => {
  assert_throws_dom_name("NamespaceError", () => {
    document.createElementNS(null, "svg:rect");
  }, "prefix with null namespace should throw NamespaceError");
}, "Document.createElementNS rejects prefixes when namespace is null");

test(() => {
  const attr = document.createAttribute("Foo");
  assert_true(attr !== null && typeof attr === "object", "createAttribute should return an object");
  assert_equals(attr.name, "foo");
  assert_equals(attr.localName, "foo");
  assert_equals(attr.namespaceURI, null);
  assert_equals(attr.prefix, null);
  assert_equals(attr.value, "");
}, "Document.createAttribute returns an Attr-like object");

test(() => {
  const attr = document.createAttribute("Foo:Bar");
  assert_equals(attr.name, "foo:bar");
  assert_equals(attr.localName, "foo:bar");
  assert_equals(attr.namespaceURI, null);
  assert_equals(attr.prefix, null);
}, "Document.createAttribute allows colons (no namespace)");

test(() => {
  const attr = document.createAttributeNS("http://example.com/ns", "p:name");
  assert_true(attr !== null && typeof attr === "object", "createAttributeNS should return an object");
  assert_equals(attr.name, "p:name");
  assert_equals(attr.localName, "name");
  assert_equals(attr.prefix, "p");
  assert_equals(attr.namespaceURI, "http://example.com/ns");
  assert_equals(attr.value, "");
}, "Document.createAttributeNS returns an Attr-like object with namespace/prefix");
