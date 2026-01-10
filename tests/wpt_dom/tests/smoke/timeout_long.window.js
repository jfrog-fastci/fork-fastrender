// META: script=/resources/testharness.js
// META: timeout=long

// Intentionally non-zero to ensure the runner honors META timeout=long when its default timeout
// is configured to be very short (validated in Rust integration tests).
async_test((t) => {
  setTimeout(
    t.step_func_done(() => {
      assert_true(true);
    }),
    50
  );
}, "META timeout=long overrides the runner default timeout");
