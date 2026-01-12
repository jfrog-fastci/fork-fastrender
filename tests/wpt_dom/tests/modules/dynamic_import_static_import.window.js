// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js

promise_test(
  () => {
    return import("/resources/mod_static_import_root.js").then((m) => {
      assert_equals(m.default, 42);
    });
  },
  "dynamic import loads a module that uses static imports"
);

