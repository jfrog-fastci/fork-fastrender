// META: script=/resources/testharness.js

// Subset of upstream WPT coverage for DOM "valid qualified name" validation.

test(() => {
  assert_throws_dom("InvalidCharacterError", () => {
    document.createElement("");
  });
}, "Document.createElement throws InvalidCharacterError for the empty string");

test(() => {
  let exception = null;
  try {
    document.createElement("a b");
  } catch (e) {
    exception = e;
  }
  assert_true(exception !== null, "expected createElement to throw");
  assert_true(exception instanceof DOMException, "expected a DOMException instance");
  assert_equals(
    Object.getPrototypeOf(exception),
    DOMException.prototype,
    "expected DOMException prototype"
  );
  assert_equals(exception.name, "InvalidCharacterError", "expected an InvalidCharacterError");
}, "Document.createElement throws a DOMException InvalidCharacterError for names containing ASCII whitespace");

test(() => {
  assert_throws_dom("InvalidCharacterError", () => {
    document.createElement("a<b");
  });
}, "Document.createElement throws InvalidCharacterError for names containing '<'");

test(() => {
  assert_throws_dom("InvalidCharacterError", () => {
    document.createElement("a:b:c");
  });
}, "Document.createElement throws InvalidCharacterError for names containing multiple ':' characters");
