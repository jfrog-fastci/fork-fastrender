// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Regression test: the vm-js WPT backend must use deterministic virtual time.
// `Date.now()` and `performance.now()` should both start at 0 and advance with the virtual clock
// when timers fire (without wall-clock sleeps).
//
// Note: we schedule a long timer (1000ms) after the initial 10ms timer to ensure the backend uses
// `idle_wait` fast-forward semantics rather than waiting in real time.

async_test((t) => {
  const start_date = Date.now();
  const start_perf = performance.now();

  assert_equals(start_date, 0, "Date.now() should start at 0");
  assert_equals(start_perf, 0, "performance.now() should start at 0");
  assert_equals(start_perf, start_date, "performance.now() should match Date.now() at start");

  setTimeout(
    t.step_func(() => {
      const now_date = Date.now();
      const now_perf = performance.now();

      assert_equals(now_date, 10, "Date.now() should be 10 in setTimeout callback");
      assert_equals(now_perf, 10, "performance.now() should be 10 in setTimeout callback");
      assert_equals(
        now_perf,
        now_date,
        "performance.now() should match Date.now() in setTimeout callback"
      );

      setTimeout(
        t.step_func_done(() => {
          const now_date_2 = Date.now();
          const now_perf_2 = performance.now();

          assert_equals(
            now_date_2,
            1010,
            "Date.now() should be 1010 in long setTimeout callback"
          );
          assert_equals(
            now_perf_2,
            1010,
            "performance.now() should be 1010 in long setTimeout callback"
          );
          assert_equals(
            now_perf_2,
            now_date_2,
            "performance.now() should match Date.now() in long setTimeout callback"
          );
        }),
        1000
      );
    }),
    10
  );
}, "Date.now()/performance.now() advance with vm-js virtual time (no wall-clock sleeps)");
