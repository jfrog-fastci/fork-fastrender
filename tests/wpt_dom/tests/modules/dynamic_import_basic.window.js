// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js

promise_test(
  () => {
    return import("/resources/mod_basic.js").then((m) => {
      assert_equals(m.default, 42);
    });
  },
  "dynamic import loads module and returns namespace"
);
