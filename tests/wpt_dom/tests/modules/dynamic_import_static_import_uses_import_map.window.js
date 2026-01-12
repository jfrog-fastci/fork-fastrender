// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
// META: script=/resources/import_map_register.js

promise_test(
  () => {
    return import("/resources/mod_static_import_uses_import_map.js").then((m) => {
      assert_equals(m.default, "mapped");
    });
  },
  "static imports resolve bare specifiers via import maps"
);

