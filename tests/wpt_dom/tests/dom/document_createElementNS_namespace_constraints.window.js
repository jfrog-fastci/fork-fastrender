// META: script=/resources/testharness.js

// Subset of upstream WPT coverage for DOM "validate and extract" (namespace/prefix checks).

test(() => {
  assert_throws_dom("InvalidCharacterError", () => {
    document.createElementNS("http://example.com/ns", "a b");
  });
}, "Document.createElementNS throws InvalidCharacterError for names containing ASCII whitespace");

test(() => {
  let exception = null;
  try {
    document.createElementNS(null, "p:local");
  } catch (e) {
    exception = e;
  }
  assert_true(exception !== null, "expected createElementNS to throw");
  assert_true(exception instanceof DOMException, "expected a DOMException instance");
  assert_equals(
    Object.getPrototypeOf(exception),
    DOMException.prototype,
    "expected DOMException prototype"
  );
  assert_equals(exception.name, "NamespaceError", "expected a NamespaceError");
}, "Document.createElementNS throws a DOMException NamespaceError for a prefixed qualifiedName with a null namespace");

test(() => {
  assert_throws_dom("NamespaceError", () => {
    document.createElementNS("http://example.com/ns", "xml:local");
  });
}, "Document.createElementNS throws NamespaceError for an 'xml' prefix with a non-XML namespace");

test(() => {
  assert_throws_dom("NamespaceError", () => {
    document.createElementNS("http://example.com/ns", "xmlns:local");
  });
}, "Document.createElementNS throws NamespaceError for an 'xmlns' prefix with a non-XMLNS namespace");

test(() => {
  assert_throws_dom("NamespaceError", () => {
    document.createElementNS("http://www.w3.org/2000/xmlns/", "local");
  });
}, "Document.createElementNS throws NamespaceError for the XMLNS namespace without an 'xmlns' prefix or qualifiedName");

test(() => {
  assert_throws_dom("NamespaceError", () => {
    document.createElementNS("http://example.com/ns", "xmlns");
  });
}, "Document.createElementNS throws NamespaceError for qualifiedName 'xmlns' with a non-XMLNS namespace");
