// Minimal `testharness.js`-compatible surface for runner smoke tests.
//
// This is *not* a full copy of upstream WPT's testharness.js; it's only the small subset needed
// by the curated fixtures in `tests/wpt_dom/tests/`.

globalThis.__wpt_results = [];

function test(fn, name) {
  try {
    fn();
    globalThis.__wpt_results.push({ name, status: 0 });
  } catch (e) {
    globalThis.__wpt_results.push({
      name,
      status: 1,
      message: e && e.message ? String(e.message) : String(e),
    });
  }
}

function assert_true(value, message) {
  if (!value) {
    throw new Error(message || "assert_true failed");
  }
}

function assert_equals(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(
      message ||
        ("assert_equals failed: " + String(actual) + " !== " + String(expected))
    );
  }
}

