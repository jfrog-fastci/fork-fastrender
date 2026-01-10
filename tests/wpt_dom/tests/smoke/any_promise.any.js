// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js

promise_test(() => {
  let ran = false;
  const p = Promise.resolve(42).then((v) => {
    ran = true;
    assert_equals(v, 42, "Promise should resolve to 42");
    assert_true(globalThis.__meta_dep_loaded === true, "META dependency should have executed");
    assert_equals(
      location.href,
      "https://web-platform.test/smoke/any_promise.any.js",
      "location.href should be the WPT test URL"
    );
  });

  // Then callbacks should run in a microtask, not synchronously.
  assert_false(ran, "Promise.then ran synchronously");
  return p;
}, "Promise.then runs in a microtask and META scripts execute");
