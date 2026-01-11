// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
// META: script=/resources/import_map_register.js

promise_test(
  () => {
    return import("foo").then((m) => {
      assert_equals(m.default, "mapped");
    });
  },
  "dynamic import resolves bare specifiers via import maps"
);
