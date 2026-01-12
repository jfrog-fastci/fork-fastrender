// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js

test(() => {
  assert_equals(typeof ResizeObserver, "function");
}, "ResizeObserver is exposed on Window");

test(() => {
  assert_throws_js(TypeError, () => ResizeObserver(() => {}));
}, "ResizeObserver called without new throws TypeError");

test(() => {
  const ro = new ResizeObserver(() => {});

  assert_equals(typeof ro.observe, "function");
  assert_equals(typeof ro.unobserve, "function");
  assert_equals(typeof ro.disconnect, "function");
  assert_equals(typeof ro.takeRecords, "function");

  assert_throws_js(TypeError, () => ro.observe(document));
  ro.observe(document.body);
  ro.observe(document.body, { box: "border-box" });
  assert_throws_js(TypeError, () => ro.observe(document.body, { box: "nope" }));

  const records = ro.takeRecords();
  assert_true(Array.isArray(records));
}, "ResizeObserver instance surface is sane");

