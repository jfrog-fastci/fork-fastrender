// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js

promise_test(async () => {
  assert_true(globalThis.__meta_dep_loaded, "META dependency should have executed");
  const v = await Promise.resolve(42);
  assert_equals(v, 42);
  assert_equals(
    location.href,
    "https://web-platform.test/smoke/any_promise.any.js",
    "location.href should be the WPT test URL"
  );
}, ".any.js promise_test + META script smoke test");

