// META: script=/resources/testharness.js
//
// Smoke test for `assert_throws_js`.

test(() => {
  const err = assert_throws_js(TypeError, () => {
    throw new TypeError("x");
  });
  assert_true(err instanceof TypeError, "assert_throws_js should return the thrown value");
}, "assert_throws_js(TypeError) passes");

test(() => {
  assert_throws_js(TypeError, () => {
    throw new SyntaxError("x");
  });
}, "assert_throws_js(TypeError) fails for wrong constructor");

