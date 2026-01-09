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
// synchronous tests and collapses the file result into a single pass/fail status via the host hook
// `__fastrender_wpt_report(...)`.
//
// Note: The runner always loads this script (and `fastrender_testharness_report.js`) before each
// WPT file; we defer "pass" reporting to a microtask so all `test(...)` calls in the file have a
// chance to run first.

var __fastrender_wpt_reported = false;
var __fastrender_wpt_failed = false;
var __fastrender_wpt_complete_scheduled = false;

function __fastrender_wpt_fail(message) {
  if (__fastrender_wpt_reported === true) return;
  __fastrender_wpt_failed = true;
  __fastrender_wpt_reported = true;
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function __fastrender_wpt_complete() {
  if (__fastrender_wpt_reported === true) return;
  if (__fastrender_wpt_failed === true) return;
  __fastrender_wpt_reported = true;
  __fastrender_wpt_report({ file_status: "pass" });
}

function test(fn, _name) {
  if (__fastrender_wpt_complete_scheduled !== true) {
    __fastrender_wpt_complete_scheduled = true;
    queueMicrotask(__fastrender_wpt_complete);
  }

  if (__fastrender_wpt_reported === true) return;

  try {
    fn();
  } catch (e) {
    // Try to surface `e.name` when possible (e.g. SyntaxError), but keep the shim tiny and avoid
    // relying on String built-ins (not supported by the vm-js runner yet).
    var msg = e;
    try {
      msg = e.name;
    } catch (_e) {}
    __fastrender_wpt_fail(msg);
  }
}
