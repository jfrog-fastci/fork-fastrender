test(() => {
  assert_not_equals(1, 2, "1 should not equal 2");
  assert_array_equals([1, 2], [1, 2], "arrays should match");
}, "testharness assertion helpers: assert_not_equals/assert_array_equals");

test(() => {
  assert_throws_js(TypeError, () => {
    throw new TypeError("x");
  });
}, "testharness assertion helpers: assert_throws_js");

