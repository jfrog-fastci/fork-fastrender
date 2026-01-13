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
  // SameValue equality (as used by upstream WPT `testharness.js`).
  //
  // Prefer native `Object.is` when available, but keep a small fallback for minimal JS backends.
  try {
    if (
      typeof Object !== "undefined" &&
      Object !== null &&
      typeof Object.is === "function"
    ) {
      return Object.is(x, y);
    }
  } catch (_e) {}
  //
  if (x === y) {
    // Distinguish +0 and -0.
    if (x === 0) return 1 / x === 1 / y;
    return true;
  }
  // NaN is SameValue-equal to itself.
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
function __safe_string(value) {
  // Avoid relying on a global `String(...)` binding (not provided by the vm-js backend).
  try {
    return ["", value].join("");
  } catch (_e) {
    try {
      return "[unstringifiable]";
    } catch (_e2) {
      return "";
    }
  }
}
//
function __format_assertion_message(user_message, auto_message) {
  if (user_message === undefined || user_message === null || user_message === "") {
    return auto_message;
  }
  return [__safe_string(user_message), ": ", auto_message].join("");
}
//
function __function_name(fn) {
  try {
    if (fn && typeof fn === "function" && typeof fn.name === "string" && fn.name !== "") {
      return fn.name;
    }
  } catch (_e) {}
  return null;
}
//
function __exception_name(err) {
  try {
    if (err && typeof err === "object") {
      if (typeof err.name === "string" && err.name !== "") return err.name;
      if (
        err.constructor &&
        typeof err.constructor.name === "string" &&
        err.constructor.name !== ""
      ) {
        return err.constructor.name;
      }
    }
  } catch (_e) {}
  try {
    return typeof err;
  } catch (_e2) {
    return "error";
  }
}
//
function __is_array_like(value) {
  try {
    if (value === null || value === undefined) return false;
    var ty = typeof value;
    if (ty !== "object") return false;
    // Allow Arrays, NodeLists, and typed arrays (shallow `length` + index access).
    return typeof value.length === "number";
  } catch (_e) {
    return false;
  }
}
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
function assert_greater_than_equal(actual, expected, message) {
  if (!(actual >= expected)) {
    throw Error(
      __format_assertion_message(
        message,
        [
          "assert_greater_than_equal: expected ",
          __safe_string(actual),
          " to be >= ",
          __safe_string(expected),
        ].join("")
      )
    );
  }
}
//
function assert_not_equals(actual, expected, message) {
  if (__same_value(actual, expected)) {
    throw Error(
      message ||
        ["assert_not_equals: got unexpectedly equal values (", __safe_string(actual), ")"].join(
          ""
        )
    );
  }
}
//
function assert_array_equals(actual, expected, message) {
  if (!__is_array_like(actual) || !__is_array_like(expected)) {
    throw Error(message || "assert_array_equals: arguments must be array-like");
  }
  var actual_length = actual.length;
  var expected_length = expected.length;
  if (actual_length !== actual_length || expected_length !== expected_length) {
    throw Error(message || "assert_array_equals: invalid length");
  }
  if (actual_length === Infinity || expected_length === Infinity) {
    throw Error(message || "assert_array_equals: invalid length");
  }
  if (actual_length < 0 || expected_length < 0) {
    throw Error(message || "assert_array_equals: invalid length");
  }
  if (!__same_value(actual_length, expected_length)) {
    throw Error(
      message ||
        [
          "assert_array_equals: length mismatch (expected ",
          expected_length,
          ", got ",
          actual_length,
          ")",
        ].join("")
    );
  }
  for (var i = 0; i < expected_length; i++) {
    if (!__same_value(actual[i], expected[i])) {
      throw Error(
        message ||
          [
            "assert_array_equals: mismatch at index ",
            i,
            " (expected ",
            __safe_string(expected[i]),
            ", got ",
            __safe_string(actual[i]),
            ")",
          ].join("")
      );
    }
  }
}
//
function assert_throws(expected, func, message) {
  if (typeof expected === "function") {
    return assert_throws_js(expected, func, message);
  }
  if (typeof expected === "string") {
    return assert_throws_dom(expected, func, message);
  }
  throw Error(message || "assert_throws: expected must be a constructor or DOMException name");
}
//
function assert_not_throws(func, message) {
  if (typeof func !== "function") {
    throw Error(message || "assert_not_throws: function is not callable");
  }
  try {
    func();
  } catch (e) {
    throw Error(
      __format_assertion_message(
        message,
        [
          "assert_not_throws: unexpected exception thrown (",
          __safe_string(e),
          ")",
        ].join("")
      )
    );
  }
}
//
function assert_throws_js(constructor, func, message) {
  if (typeof constructor !== "function") {
    throw Error(message || "assert_throws_js: expected constructor is not a function");
  }
  if (typeof func !== "function") {
    throw Error(message || "assert_throws_js: function is not callable");
  }
  //
  var threw = false;
  var thrown = null;
  try {
    func();
  } catch (e) {
    threw = true;
    thrown = e;
  }
  //
  var expected_name = __function_name(constructor);
  if (expected_name === null) expected_name = "Error";
  //
  if (threw !== true) {
    throw Error(
      __format_assertion_message(
        message,
        [
          "assert_throws_js: expected ",
          expected_name,
          " to be thrown, but no exception was thrown",
        ].join("")
      )
    );
  }
  //
  var ok = false;
  try {
    ok = thrown instanceof constructor;
  } catch (_e2) {
    ok = false;
  }
  if (ok !== true) {
    var actual_name = __exception_name(thrown);
    throw Error(
      __format_assertion_message(
        message,
        ["assert_throws_js: expected ", expected_name, ", got ", actual_name].join("")
      )
    );
  }
  return thrown;
}
//
// Legacy DOMException code string -> modern DOMException name.
//
// Upstream WPT tests still use the historical "FOO_ERR" names in many places (e.g. Range tests
// expect `assert_throws_dom("INDEX_SIZE_ERR", ...)`). Modern DOMException instances use names like
// "IndexSizeError", so we translate legacy constants to their modern equivalents.
var __dom_exception_legacy_name_map = {
  INDEX_SIZE_ERR: "IndexSizeError",
  DOMSTRING_SIZE_ERR: "DOMStringSizeError",
  HIERARCHY_REQUEST_ERR: "HierarchyRequestError",
  WRONG_DOCUMENT_ERR: "WrongDocumentError",
  INVALID_CHARACTER_ERR: "InvalidCharacterError",
  NO_DATA_ALLOWED_ERR: "NoDataAllowedError",
  NO_MODIFICATION_ALLOWED_ERR: "NoModificationAllowedError",
  NOT_FOUND_ERR: "NotFoundError",
  NOT_SUPPORTED_ERR: "NotSupportedError",
  INUSE_ATTRIBUTE_ERR: "InUseAttributeError",
  INVALID_STATE_ERR: "InvalidStateError",
  SYNTAX_ERR: "SyntaxError",
  INVALID_MODIFICATION_ERR: "InvalidModificationError",
  NAMESPACE_ERR: "NamespaceError",
  INVALID_ACCESS_ERR: "InvalidAccessError",
  VALIDATION_ERR: "ValidationError",
  TYPE_MISMATCH_ERR: "TypeMismatchError",
  SECURITY_ERR: "SecurityError",
  NETWORK_ERR: "NetworkError",
  ABORT_ERR: "AbortError",
  URL_MISMATCH_ERR: "URLMismatchError",
  QUOTA_EXCEEDED_ERR: "QuotaExceededError",
  TIMEOUT_ERR: "TimeoutError",
  INVALID_NODE_TYPE_ERR: "InvalidNodeTypeError",
  DATA_CLONE_ERR: "DataCloneError",
};
//
function assert_throws_dom(name, target, func, message) {
  // Support both upstream call patterns:
  //   assert_throws_dom("InvalidStateError", () => { ... }, "optional message")
  //   assert_throws_dom("InvalidStateError", someGlobal, () => { ... }, "optional message")
  //
  // The offline runner executes in a single realm, so the `target` is currently ignored. We keep
  // it solely for compatibility with imported WPT tests.
  var resolved_func = func;
  var resolved_message = message;
  if (typeof resolved_func !== "function") {
    resolved_func = target;
    resolved_message = func;
  }
  //
  if (typeof name !== "string") {
    throw Error(resolved_message || "assert_throws_dom: expected DOMException name must be a string");
  }
  if (typeof resolved_func !== "function") {
    throw Error(resolved_message || "assert_throws_dom: function is not callable");
  }
  //
  var expected_name = name;
  var expected_for_message = name;
  try {
    var mapped_name = __dom_exception_legacy_name_map[name];
    if (typeof mapped_name === "string") {
      expected_name = mapped_name;
      expected_for_message = [name, " (", mapped_name, ")"].join("");
    }
  } catch (_e0) {}
  //
  var threw = false;
  var thrown = null;
  try {
    resolved_func();
  } catch (e) {
    threw = true;
    thrown = e;
  }
  //
  if (threw !== true) {
    throw Error(
      __format_assertion_message(
        resolved_message,
        [
          "assert_throws_dom: expected DOMException \"",
          expected_for_message,
          "\", but no exception was thrown",
        ].join("")
      )
    );
  }
  //
  var thrown_name = null;
  try {
    if (thrown && typeof thrown === "object") {
      if (typeof thrown.name === "string") {
        thrown_name = thrown.name;
      }
    }
  } catch (_e2) {
    thrown_name = null;
  }
  //
  var ok = false;
  if (thrown_name === expected_name) ok = true;
  // If the environment still uses legacy DOMException names, accept those too.
  if (ok !== true && expected_name !== name && thrown_name === name) ok = true;
  if (ok !== true) {
    var actual_name = thrown_name;
    if (actual_name === null) {
      actual_name = __exception_name(thrown);
    }
    throw Error(
      __format_assertion_message(
        resolved_message,
        [
          "assert_throws_dom: expected DOMException \"",
          expected_for_message,
          "\", got \"",
          __safe_string(actual_name),
          "\"",
        ].join("")
      )
    );
  }
  //
  return thrown;
}
//
function assert_unreached(message) {
  throw Error(message || "assert_unreached");
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
