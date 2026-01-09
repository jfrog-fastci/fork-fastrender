// META: script=/resources/testharness.js
// META: timeout=long

async_test((t) => {
  // Intentionally non-zero to ensure the runner honors META timeout=long when its default timeout
  // is configured to be very short (validated in Rust integration tests).
  setTimeout(
    t.step_func_done(() => {
      assert_true(true, "timer fired");
    }),
    50
  );
}, "META timeout=long allows longer-running tests");

