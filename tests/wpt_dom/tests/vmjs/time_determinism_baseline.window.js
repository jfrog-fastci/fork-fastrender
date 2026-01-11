// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Baseline for `crates/js-wpt-dom-runner/tests/vmjs_time_determinism.rs`.
//
// This test is intentionally short: it runs a single 10ms timer and asserts that `Date.now()` and
// `performance.now()` start at 0 and advance deterministically to 10 when the timer fires.
//
// The Rust integration test runs this first to warm up the vm-js backend before executing
// `vmjs/time_determinism.window.js` under a wall-clock timeout.

function assert_virtual_clock(expected, context) {
  var now_date = Date.now();
  var now_perf = performance.now();

  assert_equals(now_date, expected, context);
  assert_equals(now_perf, expected, context);
  assert_equals(now_perf, now_date, context);
}

function on_time_determinism_baseline_timeout_10() {
  assert_virtual_clock(
    10,
    "virtual clock should read 10 in 10ms timer callback (baseline)"
  );
}

function run_time_determinism_baseline_test(t) {
  assert_virtual_clock(0, "virtual clock should start at 0 (baseline)");
  setTimeout(t.step_func_done(on_time_determinism_baseline_timeout_10), 10);
}

async_test(
  run_time_determinism_baseline_test,
  "Date.now()/performance.now() advance with vm-js virtual time (baseline)"
);
