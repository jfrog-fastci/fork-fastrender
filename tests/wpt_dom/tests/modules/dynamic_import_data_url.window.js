// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js

promise_test(
  () => {
    // Keep this entirely offline: `data:` URL module source is embedded in the specifier.
    //
    // Note: `Url::parse` rejects unescaped spaces, so percent-encode the payload.
    const url = "data:text/javascript,export%20default%2042%3B";
    return import(url).then((m) => {
      assert_equals(m.default, 42);
    });
  },
  "dynamic import loads data: URL modules"
);

