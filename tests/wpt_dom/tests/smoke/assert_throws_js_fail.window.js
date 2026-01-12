test(() => {
  // This test is intentionally expected to fail (xfail) to ensure that harness-side assertion
  // failures are surfaced correctly, and to guard against `assert_throws_js` erroneously passing
  // when the callback does not throw.
  assert_throws_js(TypeError, () => {});
}, "intentional failing smoke test: assert_throws_js should fail when no exception is thrown");

