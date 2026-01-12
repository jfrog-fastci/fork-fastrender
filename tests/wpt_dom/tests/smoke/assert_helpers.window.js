test(() => {
  assert_not_equals(1, 2, "1 should not equal 2");
  assert_array_equals([1, 2], [1, 2], "arrays should match");
}, "testharness assertion helpers: assert_not_equals/assert_array_equals");

test(() => {
  assert_throws_js(TypeError, () => {
    throw new TypeError("x");
  });
}, "testharness assertion helpers: assert_throws_js");

test(() => {
  // Upstream WPT assertions use SameValue, so +0 and -0 are NOT equal.
  assert_not_equals(-0, 0, "-0 should not equal +0 (SameValue)");
  assert_throws_js(Error, () => {
    assert_equals(-0, 0, "assert_equals should fail for -0 vs +0");
  });
  assert_throws_js(Error, () => {
    assert_array_equals([-0], [0], "assert_array_equals should fail for -0 vs +0");
  });
}, "testharness SameValue semantics (+0/-0 distinction)");
