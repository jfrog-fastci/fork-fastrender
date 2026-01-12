// META: script=/resources/testharness.js

// Subset of upstream WPT coverage for Element.setAttribute qualified name validation.

test(() => {
  const el = document.createElement("div");
  assert_throws_dom("InvalidCharacterError", () => {
    el.setAttribute("", "x");
  });
}, "Element.setAttribute throws InvalidCharacterError for the empty string");

test(() => {
  const el = document.createElement("div");
  let exception = null;
  try {
    el.setAttribute("a b", "x");
  } catch (e) {
    exception = e;
  }
  assert_true(exception !== null, "expected setAttribute to throw");
  assert_true(exception instanceof DOMException, "expected a DOMException instance");
  assert_equals(exception.name, "InvalidCharacterError", "expected an InvalidCharacterError");
}, "Element.setAttribute throws a DOMException InvalidCharacterError for names containing ASCII whitespace");

test(() => {
  const el = document.createElement("div");
  assert_throws_dom("InvalidCharacterError", () => {
    el.setAttribute("a<b", "x");
  });
}, "Element.setAttribute throws InvalidCharacterError for names containing '<'");

test(() => {
  const el = document.createElement("div");
  assert_throws_dom("InvalidCharacterError", () => {
    el.setAttribute("a:b:c", "x");
  });
}, "Element.setAttribute throws InvalidCharacterError for names containing multiple ':' characters");
