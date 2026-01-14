// META: script=/resources/testharness.js
// META: script=/dom/support/setup_noop.js
// META: script=/dom/common.js
//
// Regression tests for `getDomExceptionName()` in `dom/common.js`.
//
// FastRender's offline DOM runner may throw DOMException-like objects that only expose the standard
// `.name` string (no legacy numeric `.code` and no `*_ERR` constants). These tests ensure the helper
// doesn't throw while attempting to map legacy codes and keeps legacy behavior when the mapping is
// available.

test(() => {
  assert_equals(getDomExceptionName({ name: "InvalidStateError" }), "InvalidStateError");
}, "getDomExceptionName returns `.name` for DOMException-like objects without `.code`");

test(() => {
  assert_equals(
    getDomExceptionName({ name: "InvalidStateError", code: 11, INVALID_STATE_ERR: 11 }),
    "INVALID_STATE_ERR"
  );
}, "getDomExceptionName preserves legacy `*_ERR` mapping when `.code` + constants are present");

test(() => {
  assert_equals(
    getDomExceptionName({ name: "InvalidStateError", code: 11 }),
    "InvalidStateError"
  );
}, "getDomExceptionName falls back to `.name` when `.code` is present but constants are missing");
