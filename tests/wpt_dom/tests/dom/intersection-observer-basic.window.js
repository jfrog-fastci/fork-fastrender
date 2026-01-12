// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js

test(() => {
  assert_equals(typeof IntersectionObserver, "function");
}, "IntersectionObserver is exposed on Window");

test(() => {
  assert_throws_js(TypeError, () => IntersectionObserver(() => {}));
}, "IntersectionObserver called without new throws TypeError");

test(() => {
  const io = new IntersectionObserver(() => {});

  assert_true(Array.isArray(io.thresholds));
  assert_greater_than_equal(io.thresholds.length, 1);

  assert_equals(io.root, null);
  assert_equals(typeof io.rootMargin, "string");

  assert_equals(typeof io.observe, "function");
  assert_equals(typeof io.unobserve, "function");
  assert_equals(typeof io.disconnect, "function");
  assert_equals(typeof io.takeRecords, "function");

  assert_throws_js(TypeError, () => io.observe(document));
  io.observe(document.body);

  const records = io.takeRecords();
  assert_true(Array.isArray(records));
}, "IntersectionObserver instance surface is sane");

