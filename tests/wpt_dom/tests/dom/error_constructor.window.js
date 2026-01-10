// META: script=/resources/testharness.js
//
// Basic sanity: `Error` should exist and be constructible so tests can throw failures using
// `new Error(...)`.

test(() => {
  let err = null;
  try {
    err = new Error("boom");
  } catch (_e) {
    assert_true(false, "new Error(...) should not throw");
  }

  assert_true(err !== null, "new Error(...) should return an object");
  assert_equals(err.name, "Error", "Error instance name should be 'Error'");
  assert_equals(err.message, "boom", "Error instance message should match constructor argument");
}, "Error is constructible and sets name/message");

test(() => {
  const err = new Error("boom");
  let caught = null;
  try {
    throw err;
  } catch (e) {
    caught = e;
  }
  assert_equals(caught, err, "throw/catch should preserve the thrown Error object");
}, "throw/catch preserves Error object identity");
