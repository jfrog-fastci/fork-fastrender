// Minimal deterministic subset of WPT `testharness.js` for FastRender's offline DOM runner.
//
// The upstream `testharness.js` is large; FastRender only needs a small, spec-shaped subset:
//
// - synchronous tests (`test`)
// - async tests (`async_test`) with `t.done()`, `t.step_func(cb)`, and `t.step_func_done(cb)`
// - promise tests (`promise_test`)
// - reporter callbacks (`add_result_callback`, `add_completion_callback`)
//
// Reporting is *entirely* callback-driven. This shim must not call `__fastrender_wpt_report`
// directly; `resources/fastrender_testharness_report.js` is responsible for producing the final
// host report payload.
//
// Note: Keep this file compatible with FastRender's in-tree vm-js backend (avoid arithmetic
// operators like `+`/`-` and avoid closures).
//
// WPT status constants.
var PASS = 0;
var FAIL = 1;
var TIMEOUT = 2;
var NOTRUN = 3;
//
// Reporter callback registries.
var __result_callbacks = [];
var __completion_callbacks = [];
//
// Test records surfaced to reporters.
var __tests = [];
//
// Number of pending async/promise tests.
var __pending = 0;
//
// Script completion tracking: the runner performs a microtask checkpoint after each evaluated
// script. We schedule a microtask from the first test registration so `__script_done` flips only
// after the test file finishes executing.
var __script_done = false;
var __script_done_scheduled = false;
var __reported_completion = false;
//
// Harness status object passed to completion callbacks (shape-compatible with upstream).
var __harness_status = { status: 0, message: null, stack: null };
//
function add_result_callback(fn) {
  if (typeof fn !== "function") return;
  __result_callbacks.push(fn);
}
//
function add_completion_callback(fn) {
  if (typeof fn !== "function") return;
  __completion_callbacks.push(fn);
}
//
function __queue_microtask(cb) {
  if (typeof queueMicrotask === "function") {
    queueMicrotask(cb);
    return;
  }
  //
  // Fallback for partial environments: promise jobs are microtasks.
  try {
    if (
      typeof Promise !== "undefined" &&
      Promise !== null &&
      typeof Promise.resolve === "function"
    ) {
      Promise.resolve().then(cb);
      return;
    }
  } catch (_e) {}
  //
  // Last resort fallback: schedule a task.
  if (typeof setTimeout === "function") {
    setTimeout(cb, 0);
  } else {
    cb();
  }
}
//
function __schedule_script_done() {
  if (__script_done_scheduled === true) return;
  __script_done_scheduled = true;
  __queue_microtask(__mark_script_done);
}
//
function __same_value(x, y) {
  // Minimal SameValue: strict equality, plus NaN equality.
  //
  // Note: This intentionally treats +0 and -0 as equal. The curated offline corpus does not
  // currently rely on the distinction, and keeping this shim arithmetic-free makes it runnable on
  // the minimal vm-js backend.
  if (x === y) return true;
  return x !== x && y !== y;
}
//
function __error_to_message(err) {
  // Prefer `.message` when present (Error-like objects).
  try {
    if (err && typeof err === "object" && typeof err.message === "string") {
      return err.message;
    }
  } catch (_e) {}
  //
  // If the thrown value is already a string, surface it directly.
  try {
    if (typeof err === "string") return err;
  } catch (_e) {}
  //
  return "error";
}
//
function __error_to_stack(err) {
  try {
    if (err && typeof err === "object" && typeof err.stack === "string") {
      return err.stack;
    }
  } catch (_e) {}
  return null;
}
//
function __fail_test_record(t, err) {
  t.status = FAIL;
  t.message = __error_to_message(err);
  t.stack = __error_to_stack(err);
}
//
function __report_test_result(t) {
  for (var i = 0; i !== __result_callbacks.length; i++) {
    __result_callbacks[i](t);
  }
}
//
function __check_complete() {
  if (__reported_completion === true) return;
  if (__script_done !== true) return;
  if (__pending !== 0) return;
  //
  __reported_completion = true;
  for (var i = 0; i !== __completion_callbacks.length; i++) {
    __completion_callbacks[i](__tests, __harness_status);
  }
}
//
function __make_test_record(name) {
  var resolved_name = name;
  if (resolved_name === undefined || resolved_name === null || resolved_name === "") {
    resolved_name = "(unnamed)";
  }
  return {
    name: resolved_name,
    status: NOTRUN,
    message: null,
    stack: null,
    // Internal bookkeeping.
    _done: false,
  };
}
//
function __push_test_record(t) {
  __tests.push(t);
  __schedule_script_done();
}
//
function __mark_script_done() {
  __script_done = true;
  __check_complete();
}
//
// ---------------------------------------------------------------------------
// Assertions (minimal subset used by the curated corpus).
//
function assert_true(value, message) {
  if (value !== true) {
    throw Error(message || "assert_true");
  }
}
//
function assert_false(value, message) {
  if (value !== false) {
    throw Error(message || "assert_false");
  }
}
//
function assert_equals(actual, expected, message) {
  if (!__same_value(actual, expected)) {
    throw Error(message || "assert_equals");
  }
}
//
function assert_unreached(message) {
  throw Error(message || "assert_unreached");
}
//
function assert_throws_dom(expected_name, target, fn, message) {
  // Minimal subset of upstream WPT `assert_throws_dom` used by the offline DOM corpus.
  //
  // Supported call patterns:
  //   assert_throws_dom("InvalidCharacterError", () => { ... })
  //   assert_throws_dom("InvalidCharacterError", someGlobal, () => { ... })
  //
  // Note: The second form exists in upstream WPT so tests can assert the exception comes from the
  // correct global object; the offline runner executes in a single realm, but we keep it for
  // compatibility with imported tests.
  var resolved_target = target;
  var resolved_fn = fn;
  var resolved_message = message;
  //
  if (typeof target === "function") {
    resolved_target = null;
    resolved_fn = target;
    resolved_message = fn;
  }
  //
  if (typeof resolved_fn !== "function") {
    throw Error(resolved_message || "assert_throws_dom: callback is not callable");
  }
  //
  var threw = false;
  var exception = null;
  try {
    resolved_fn();
  } catch (e) {
    threw = true;
    exception = e;
  }
  //
  if (threw !== true) {
    throw Error(resolved_message || "assert_throws_dom: did not throw");
  }
  //
  // Resolve the DOMException constructor (prefer the supplied target, then fall back to the global
  // binding). If DOMException does not exist yet, this assertion should fail loudly so the test
  // suite encodes the DOMException bring-up requirement.
  var dom_exception_ctor = null;
  try {
    if (
      resolved_target !== null &&
      resolved_target !== undefined &&
      resolved_target.DOMException !== undefined &&
      resolved_target.DOMException !== null
    ) {
      dom_exception_ctor = resolved_target.DOMException;
    }
  } catch (_e) {}
  //
  if (dom_exception_ctor === null) {
    try {
      if (typeof DOMException === "function") dom_exception_ctor = DOMException;
    } catch (_e) {}
  }
  //
  if (dom_exception_ctor === null) {
    throw Error(resolved_message || "assert_throws_dom: DOMException is not defined");
  }
  //
  if (!(exception instanceof dom_exception_ctor)) {
    throw Error(resolved_message || "assert_throws_dom: not a DOMException");
  }
  //
  var actual_name = "";
  try {
    actual_name = exception.name;
  } catch (_e) {}
  //
  if (actual_name !== expected_name) {
    throw Error(resolved_message || "assert_throws_dom: wrong exception name");
  }
}
//
// ---------------------------------------------------------------------------
// Test entry points.
//
function test(fn, name) {
  var t = __make_test_record(name);
  __push_test_record(t);
  //
  try {
    fn();
    t.status = PASS;
  } catch (e) {
    __fail_test_record(t, e);
  }
  //
  __report_test_result(t);
  __check_complete();
  return t;
}
//
function async_test(fn, name) {
  if (typeof fn === "string" && name === undefined) {
    name = fn;
    fn = null;
  }
  //
  var t = __make_test_record(name);
  __push_test_record(t);
  __pending++;
  //
  // Assign methods without relying on function expressions (the minimal vm-js backend only supports
  // arrow functions as expressions).
  t.done = __async_test_done;
  t.step_func = __async_test_step_func;
  t.step_func_done = __async_test_step_func_done;
  //
  if (typeof fn === "function") {
    try {
      fn(t);
    } catch (e) {
      __fail_test_record(t, e);
      t.done();
    }
  }
  //
  return t;
}
//
function promise_test(fn, name) {
  var t = __make_test_record(name);
  __push_test_record(t);
  __pending++;
  //
  // Minimal promise_test plumbing without relying on closures: store the current test record in a
  // global slot so the shared fulfill/reject handlers can resolve it.
  __promise_test_current = t;
  //
  try {
    var p = fn();
    if (!p || typeof p.then !== "function") {
      __promise_test_rejected(Error("promise_test: returned value is not a Promise"));
      return t;
    }
    p.then(__promise_test_fulfilled, __promise_test_rejected);
  } catch (e) {
    __promise_test_rejected(e);
  }
  //
  return t;
}
//
// ---------------------------------------------------------------------------
// Minimal async helpers (closure-free).
//
function __async_test_done() {
  var t = this;
  if (t._done === true) return;
  t._done = true;
  //
  if (t.status === NOTRUN) {
    t.status = PASS;
  }
  //
  __pending--;
  __report_test_result(t);
  __check_complete();
}
//
// Note: This harness deliberately avoids closures to stay compatible with the in-tree vm-js
// backend, so `step_func`/`step_func_done` use a single global slot. This means only one wrapped
// callback per helper may be outstanding at a time.
//
// This is sufficient for the curated offline corpus (each test file contains at most one async
// test and does not schedule multiple wrapped callbacks concurrently).
var __step_func_test = null;
var __step_func_callback = null;
//
function __async_test_step_func(cb) {
  __step_func_test = this;
  __step_func_callback = cb;
  return __async_test_step_func_wrapper;
}
//
function __async_test_step_func_wrapper(a0, a1, a2, a3) {
  var t = __step_func_test;
  if (!t || t._done === true) return;
  //
  try {
    if (typeof __step_func_callback === "function") {
      __step_func_callback(a0, a1, a2, a3);
    } else {
      __fail_test_record(t, Error("step_func: callback is not callable"));
      t.done();
    }
  } catch (e) {
    __fail_test_record(t, e);
    t.done();
  }
}
//
var __step_func_done_test = null;
var __step_func_done_callback = null;
//
function __async_test_step_func_done(cb) {
  __step_func_done_test = this;
  __step_func_done_callback = cb;
  return __async_test_step_func_done_wrapper;
}
//
function __async_test_step_func_done_wrapper(a0, a1, a2, a3) {
  var t = __step_func_done_test;
  if (!t || t._done === true) return;
  //
  try {
    if (typeof __step_func_done_callback === "function") {
      __step_func_done_callback(a0, a1, a2, a3);
    } else {
      __fail_test_record(t, Error("step_func_done: callback is not callable"));
    }
  } catch (e) {
    __fail_test_record(t, e);
  }
  //
  t.done();
}
//
var __promise_test_current = null;
//
function __promise_test_fulfilled(_value) {
  var t = __promise_test_current;
  if (!t || t._done === true) return;
  t._done = true;
  t.status = PASS;
  __pending--;
  __report_test_result(t);
  __check_complete();
}
//
function __promise_test_rejected(reason) {
  var t = __promise_test_current;
  if (!t || t._done === true) return;
  t._done = true;
  __fail_test_record(t, reason);
  __pending--;
  __report_test_result(t);
  __check_complete();
}
