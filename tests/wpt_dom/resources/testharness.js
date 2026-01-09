// Minimal `testharness.js` shim for FastRender's offline DOM WPT runner.
//
// This file is intentionally tiny: the current `vm-js`-backed runner executes only a small
// JavaScript subset, and the curated smoke tests use the `__fastrender_wpt_report(...)` hook
// directly.
//
// As the JS engine grows, we can replace this with a closer upstream `testharness.js` copy.

var PASS = 0;
var FAIL = 1;
var TIMEOUT = 2;
var NOTRUN = 3;

function assert_true(value) {
  if (value !== true) {
    throw "assert_true";
  }
}

function assert_false(value) {
  if (value !== false) {
    throw "assert_false";
  }
}

function assert_equals(actual, expected) {
  if (actual !== expected) {
    throw "assert_equals";
  }
}

// ---------------------------------------------------------------------------
// Minimal synchronous `test()` support.
//
// The upstream WPT `testharness.js` supports asynchronous tests, subtests, and rich reporting.
// FastRender's in-tree `vm-js` backend is still a small interpreter, so this shim only supports
// a small subset of the API surface and collapses the file result into a single pass/fail status
// via the host hook `__fastrender_wpt_report(...)`.
//
// Note: The runner always loads this script (and `fastrender_testharness_report.js`) before each
// WPT file; we defer "pass" reporting to a microtask so all `test(...)` calls in the file have a
// chance to run first.

var __fastrender_wpt_reported = false;
var __fastrender_wpt_failed = false;
var __fastrender_wpt_script_done = false;
var __fastrender_wpt_script_done_scheduled = false;
var __fastrender_wpt_pending = false;
var __fastrender_wpt_async_step_fn = null;

function __fastrender_wpt_fail(message) {
  if (__fastrender_wpt_reported === true) return;
  __fastrender_wpt_failed = true;
  __fastrender_wpt_reported = true;
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function __fastrender_wpt_maybe_complete() {
  if (__fastrender_wpt_reported === true) return;
  if (__fastrender_wpt_failed === true) return;
  if (__fastrender_wpt_script_done !== true) return;
  if (__fastrender_wpt_pending === true) return;
  __fastrender_wpt_reported = true;
  __fastrender_wpt_report({ file_status: "pass" });
}

function __fastrender_wpt_mark_script_done() {
  __fastrender_wpt_script_done = true;
  __fastrender_wpt_maybe_complete();
}

function __fastrender_wpt_schedule_script_done() {
  if (__fastrender_wpt_script_done_scheduled === true) return;
  __fastrender_wpt_script_done_scheduled = true;
  queueMicrotask(__fastrender_wpt_mark_script_done);
}

function __fastrender_wpt_error_message(e) {
  return e;
}

function test(fn, _name) {
  __fastrender_wpt_schedule_script_done();

  if (__fastrender_wpt_reported === true) return;

  try {
    fn();
  } catch (e) {
    // Encode the test name in the thrown value without depending on full stack trace support.
    __fastrender_wpt_fail({ name: _name, message: __fastrender_wpt_error_message(e) });
  }
}

// ---------------------------------------------------------------------------
// Minimal async_test/promise_test support.
//
// The vm-js-backed runner doesn't support enough JavaScript (e.g. closures, arithmetic operators)
// to run upstream WPT testharness in full. These helpers are just enough for the curated offline
// smoke tests under `tests/wpt_dom/tests/smoke`.

function __fastrender_wpt_async_done() {
  if (__fastrender_wpt_reported === true) return;
  __fastrender_wpt_pending = false;
  __fastrender_wpt_maybe_complete();
}

function __fastrender_wpt_async_step_wrapper() {
  if (__fastrender_wpt_reported === true) return;
  try {
    if (__fastrender_wpt_async_step_fn) {
      __fastrender_wpt_async_step_fn();
    }
  } catch (e) {
    __fastrender_wpt_fail(__fastrender_wpt_error_message(e));
  }
  __fastrender_wpt_async_done();
}

function __fastrender_wpt_async_step_func_done(fn) {
  __fastrender_wpt_async_step_fn = fn;
  return __fastrender_wpt_async_step_wrapper;
}

function async_test(fn, _name) {
  __fastrender_wpt_schedule_script_done();
  if (__fastrender_wpt_reported === true) return;

  __fastrender_wpt_pending = true;
  var t = {
    done: __fastrender_wpt_async_done,
    step_func_done: __fastrender_wpt_async_step_func_done
  };

  try {
    fn(t);
  } catch (e) {
    __fastrender_wpt_fail(__fastrender_wpt_error_message(e));
    __fastrender_wpt_async_done();
  }

  return t;
}

function __fastrender_wpt_promise_fulfilled(_value) {
  __fastrender_wpt_async_done();
}

function __fastrender_wpt_promise_rejected(reason) {
  __fastrender_wpt_fail(__fastrender_wpt_error_message(reason));
  __fastrender_wpt_async_done();
}

function promise_test(fn, _name) {
  __fastrender_wpt_schedule_script_done();
  if (__fastrender_wpt_reported === true) return;

  __fastrender_wpt_pending = true;
  try {
    var p = fn();
    p.then(__fastrender_wpt_promise_fulfilled, __fastrender_wpt_promise_rejected);
  } catch (e) {
    __fastrender_wpt_fail(__fastrender_wpt_error_message(e));
    __fastrender_wpt_async_done();
  }
}
