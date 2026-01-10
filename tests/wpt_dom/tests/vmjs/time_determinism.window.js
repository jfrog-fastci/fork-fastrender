// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Regression test: the vm-js WPT backend must use deterministic virtual time.
// `Date.now()` and `performance.now()` should both start at 0 and advance with the virtual clock
// when timers fire (without wall-clock sleeps).
//
// Note: we schedule a long timer (1000ms) after the initial 10ms timer to ensure the backend uses
// `idle_wait` fast-forward semantics rather than waiting in real time.

// Keep state in globals so timer callbacks do not capture locals. This file must run under the
// legacy vm-js backend which historically had issues with closures inside timer callbacks.
var time_determinism_t = null;

function core_web_globals_exist() {
  // Ensure the runner exposes spec-shaped globals as both identifiers and global properties.
  assert_true(typeof globalThis !== "undefined", "globalThis should exist");

  assert_true(typeof globalThis.Date !== "undefined", "globalThis.Date should exist");
  assert_true(typeof Date !== "undefined", "Date identifier should exist");
  assert_equals(Date, globalThis.Date, "Date identifier should match globalThis.Date");
  assert_equals(
    typeof globalThis.Date.now,
    "function",
    "globalThis.Date.now should be a function"
  );

  assert_true(
    typeof globalThis.performance !== "undefined",
    "globalThis.performance should exist"
  );
  assert_true(typeof performance !== "undefined", "performance identifier should exist");
  assert_equals(
    performance,
    globalThis.performance,
    "performance identifier should match globalThis.performance"
  );
  assert_equals(
    typeof globalThis.performance.now,
    "function",
    "globalThis.performance.now should be a function"
  );

  assert_equals(
    typeof globalThis.setTimeout,
    "function",
    "globalThis.setTimeout should be a function"
  );
  assert_true(typeof setTimeout !== "undefined", "setTimeout identifier should exist");
  assert_equals(
    setTimeout,
    globalThis.setTimeout,
    "setTimeout identifier should match globalThis.setTimeout"
  );
}

function assert_virtual_clock(expected, context) {
  var now_date = Date.now();
  var now_perf = performance.now();

  assert_equals(now_date, expected, context);
  assert_equals(now_perf, expected, context);
  assert_equals(now_perf, now_date, context);
}

function time_determinism_error_message(err) {
  try {
    if (err && typeof err === "object" && typeof err.message === "string") {
      return err.message;
    }
  } catch (_e) {}
  try {
    if (typeof err === "string") return err;
  } catch (_e2) {}
  return "error";
}

function time_determinism_error_stack(err) {
  try {
    if (err && typeof err === "object" && typeof err.stack === "string") {
      return err.stack;
    }
  } catch (_e) {}
  return null;
}

function time_determinism_fail(err) {
  var t = time_determinism_t;
  if (!t || t._done === true) return;
  t.status = FAIL;
  t.message = time_determinism_error_message(err);
  t.stack = time_determinism_error_stack(err);
  t.done();
}

function on_time_determinism_timeout_1010() {
  assert_virtual_clock(1010, "virtual clock should read 1010 in 1000ms timer callback");
}

function on_time_determinism_timeout_10() {
  var t = time_determinism_t;
  if (!t || t._done === true) return;

  try {
    assert_virtual_clock(10, "virtual clock should read 10 in 10ms timer callback");
  } catch (e) {
    time_determinism_fail(e);
    return;
  }

  setTimeout(t.step_func_done(on_time_determinism_timeout_1010), 1000);
}

function run_time_determinism_test(t) {
  time_determinism_t = t;

  core_web_globals_exist();

  assert_virtual_clock(0, "virtual clock should start at 0");
  setTimeout(on_time_determinism_timeout_10, 10);
}

async_test(
  run_time_determinism_test,
  "Date.now()/performance.now() advance with vm-js virtual time (no wall-clock sleeps)"
);
