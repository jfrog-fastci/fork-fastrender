// META: script=/resources/testharness.js
//
// Smoke test for `assert_throws_dom`.

test(() => {
  const err = assert_throws_dom("SyntaxError", () => {
    throw new DOMException("m", "SyntaxError");
  });
  assert_equals(err.name, "SyntaxError", "assert_throws_dom should return the thrown value");
}, "assert_throws_dom(SyntaxError) passes");

test(() => {
  assert_throws_dom("SyntaxError", () => {
    throw new DOMException("m", "InvalidCharacterError");
  });
}, "assert_throws_dom(SyntaxError) fails for wrong name");

