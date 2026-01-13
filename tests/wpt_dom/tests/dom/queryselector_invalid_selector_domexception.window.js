// META: script=/resources/testharness.js

test(() => {
  let exception = null;
  try {
    document.querySelector("div[");
  } catch (e) {
    exception = e;
  }
  assert_true(exception !== null, "expected querySelector to throw");
  assert_true(exception instanceof DOMException, "expected a DOMException instance");
  assert_equals(
    Object.getPrototypeOf(exception),
    DOMException.prototype,
    "expected DOMException prototype"
  );
  assert_equals(exception.name, "SyntaxError", "expected a SyntaxError DOMException");
}, "Document.querySelector throws a DOMException SyntaxError for invalid selectors");

test(() => {
  assert_throws_dom("SyntaxError", () => {
    document.querySelector("div[");
  });
}, "assert_throws_dom works for Document.querySelector SyntaxError");

test(() => {
  const el = document.createElement("div");
  assert_throws_dom("SyntaxError", () => {
    el.querySelector("div[");
  });
}, "assert_throws_dom works for Element.querySelector SyntaxError");
