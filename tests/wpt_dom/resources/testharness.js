// Minimal deterministic subset of WPT `testharness.js` for FastRender's offline DOM runner.
//
// The upstream `testharness.js` is large; FastRender only needs a small, spec-shaped subset:
//
// - synchronous tests (`test`)
// - async tests (`async_test`) with `t.done()`, `t.step(cb)`, `t.step_func(cb)`, and
//   `t.step_func_done(cb)`
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
function __record_harness_error(err) {
  // Match upstream harness status shape: {status, message, stack}.
  __harness_status.status = 1;
  __harness_status.message = __error_to_message(err);
  __harness_status.stack = __error_to_stack(err);
}
//
function setup(func_or_props, maybe_props) {
  // Minimal `setup()` implementation used by some WPT helpers.
  //
  // Supported call patterns:
  //   setup({ ...options... })         (ignored)
  //   setup(() => { ... })             (invoked immediately)
  //   setup(() => { ... }, { ... })    (options ignored, callback invoked immediately)
  //   setup({ ... }, () => { ... })    (options ignored, callback invoked immediately)
  //
  // The full upstream harness uses `setup()` to configure harness flags and run setup callbacks
  // before tests. The curated offline corpus relies on the setup callback running synchronously.
  var func = null;
  if (typeof func_or_props === "function") {
    func = func_or_props;
  }
  if (typeof maybe_props === "function") {
    func = maybe_props;
  }
  if (func !== null) {
    try {
      func();
    } catch (e) {
      __record_harness_error(e);
    }
  }
}
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
// Upstream WPT provides `setup(fn)` as a convenience for shared harness scripts (e.g. `dom/common.js`)
// to run initialization before tests are registered. Our offline runner executes scripts
// synchronously, so we can run the callback immediately.
function setup(arg) {
  if (typeof arg === "function") {
    arg();
  }
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
function assert_approx_equals(actual, expected, epsilon, message) {
  // Minimal `assert_approx_equals` helper used by layout/geometry tests.
  //
  // Keep this conservative: accept only finite numbers.
  if (typeof actual !== "number" || typeof expected !== "number" || typeof epsilon !== "number") {
    throw Error(message || "assert_approx_equals");
  }
  if (
    actual !== actual ||
    expected !== expected ||
    epsilon !== epsilon ||
    actual === Infinity ||
    actual === -Infinity ||
    expected === Infinity ||
    expected === -Infinity ||
    epsilon === Infinity ||
    epsilon === -Infinity ||
    epsilon < 0
  ) {
    throw Error(message || "assert_approx_equals");
  }
  var diff = actual - expected;
  if (diff < 0) diff = -diff;
  if (!(diff <= epsilon)) {
    throw Error(
      __format_assertion_message(
        message,
        [
          "assert_approx_equals: expected ",
          __safe_string(actual),
          " to be within ",
          __safe_string(epsilon),
          " of ",
          __safe_string(expected),
        ].join("")
      )
    );
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
function assert_throws_dom(name, target, func, message) {
  // Support upstream call patterns:
  //   assert_throws_dom("InvalidStateError", () => { ... }, "optional message")
  //   assert_throws_dom("InvalidStateError", someGlobal, () => { ... }, "optional message")
  //   assert_throws_dom("INVALID_STATE_ERR", someGlobal.DOMException, () => { ... }, "optional message")
  //
  // The offline runner executes in a single realm, so the `target` is currently ignored. We keep
  // it solely for compatibility with imported WPT tests.
  var resolved_func = func;
  var resolved_message = message;
  //
  // Upstream supports both 3-arg and 4-arg call signatures. In particular, some Range tests call:
  //   assert_throws_dom(name, iframe.contentWindow.DOMException, () => { ... }, msg)
  // where both the 2nd and 3rd arguments are functions (constructor + callable).
  if (typeof target === "function" && typeof func !== "function") {
    // 3-arg form: assert_throws_dom(name, func, message)
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
  // Legacy DOMException code names (e.g. "HIERARCHY_REQUEST_ERR") are still used in many WPT tests,
  // especially Range tests. Modern DOMException instances use camel-case names (e.g.
  // "HierarchyRequestError"), while `.code` preserves the historical numeric values.
  //
  // Keep this mapping *inside* `assert_throws_dom` so we don't leak helper globals into the test
  // realm. However, allocate it only once: Range-heavy suites can call `assert_throws_dom` many
  // times, and repeatedly allocating these large object literals adds unnecessary GC pressure in
  // small JS backends.
  var __dom_exception_legacy_name_map =
    assert_throws_dom.__fastrender_dom_exception_legacy_name_map;
  var __dom_exception_legacy_code_map =
    assert_throws_dom.__fastrender_dom_exception_legacy_code_map;
  if (
    __dom_exception_legacy_name_map === undefined ||
    __dom_exception_legacy_code_map === undefined
  ) {
    __dom_exception_legacy_name_map = {
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
    __dom_exception_legacy_code_map = {
      INDEX_SIZE_ERR: 1,
      DOMSTRING_SIZE_ERR: 2,
      HIERARCHY_REQUEST_ERR: 3,
      WRONG_DOCUMENT_ERR: 4,
      INVALID_CHARACTER_ERR: 5,
      NO_DATA_ALLOWED_ERR: 6,
      NO_MODIFICATION_ALLOWED_ERR: 7,
      NOT_FOUND_ERR: 8,
      NOT_SUPPORTED_ERR: 9,
      INUSE_ATTRIBUTE_ERR: 10,
      INVALID_STATE_ERR: 11,
      SYNTAX_ERR: 12,
      INVALID_MODIFICATION_ERR: 13,
      NAMESPACE_ERR: 14,
      INVALID_ACCESS_ERR: 15,
      VALIDATION_ERR: 16,
      TYPE_MISMATCH_ERR: 17,
      SECURITY_ERR: 18,
      NETWORK_ERR: 19,
      ABORT_ERR: 20,
      URL_MISMATCH_ERR: 21,
      QUOTA_EXCEEDED_ERR: 22,
      TIMEOUT_ERR: 23,
      INVALID_NODE_TYPE_ERR: 24,
      DATA_CLONE_ERR: 25,
    };
    assert_throws_dom.__fastrender_dom_exception_legacy_name_map =
      __dom_exception_legacy_name_map;
    assert_throws_dom.__fastrender_dom_exception_legacy_code_map =
      __dom_exception_legacy_code_map;
  }
  //
  var expected_legacy_code = null;
  var expected_modern_name = null;
  var expected_for_message = name;
  try {
    var is_legacy_expected_name = false;
    if (typeof name.endsWith === "function") {
      is_legacy_expected_name = name.endsWith("_ERR");
    } else {
      is_legacy_expected_name = /_ERR$/.test(name);
    }
    if (is_legacy_expected_name === true) {
      var mapped_name = __dom_exception_legacy_name_map[name];
      if (typeof mapped_name === "string" && mapped_name !== "") {
        expected_modern_name = mapped_name;
        expected_for_message = [name, " (", mapped_name, ")"].join("");
      }
      var mapped_code = __dom_exception_legacy_code_map[name];
      if (typeof mapped_code === "number") {
        expected_legacy_code = mapped_code;
      }
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
  var match = false;
  if (thrown_name === name) {
    match = true;
  } else if (expected_modern_name !== null && thrown_name === expected_modern_name) {
    match = true;
  } else {
    // Also accept engines that throw the legacy name when tests expect the modern name.
    var thrown_modern_name = null;
    try {
      if (typeof thrown_name === "string") {
        var is_legacy_thrown_name = false;
        if (typeof thrown_name.endsWith === "function") {
          is_legacy_thrown_name = thrown_name.endsWith("_ERR");
        } else {
          is_legacy_thrown_name = /_ERR$/.test(thrown_name);
        }
        if (is_legacy_thrown_name === true) {
          var mapped_thrown = __dom_exception_legacy_name_map[thrown_name];
          if (typeof mapped_thrown === "string" && mapped_thrown !== "") {
            thrown_modern_name = mapped_thrown;
          }
        }
      }
    } catch (_e3) {
      thrown_modern_name = null;
    }
    if (thrown_modern_name !== null) {
      if (thrown_modern_name === name) {
        match = true;
      } else if (expected_modern_name !== null && thrown_modern_name === expected_modern_name) {
        match = true;
      }
    }
  }
  //
  if (match !== true && expected_legacy_code !== null) {
    try {
      if (thrown && typeof thrown === "object" && typeof thrown.code === "number") {
        if (thrown.code === expected_legacy_code) {
          match = true;
        }
      }
    } catch (_e3) {}
  }
  //
  if (match !== true) {
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
function assert_not_throws(func, message) {
  if (typeof func !== "function") {
    throw Error(message || "assert_not_throws: function is not callable");
  }
  try {
    return func();
  } catch (e) {
    throw Error(
      __format_assertion_message(
        message,
        ["assert_not_throws: unexpected exception (", __exception_name(e), ")"].join("")
      )
    );
  }
}
//
function assert_unreached(message) {
  throw Error(message || "assert_unreached");
}
//
// ---------------------------------------------------------------------------
// WPT helper: generate_tests(fn, cases)
//
// Many upstream DOM tests use `generate_tests` to expand a list of parameter tuples into multiple
// `test(...)` invocations. Keep this implementation compatible with the minimal vm-js backend by
// avoiding closures: `test(...)` executes synchronously, so we can use a shared wrapper and global
// argument slots.
//
var __generate_tests_func = null;
var __generate_tests_arg_count = 0;
var __generate_tests_arg0 = undefined;
var __generate_tests_arg1 = undefined;
var __generate_tests_arg2 = undefined;
var __generate_tests_arg3 = undefined;
var __generate_tests_arg4 = undefined;
var __generate_tests_arg5 = undefined;
var __generate_tests_arg6 = undefined;
var __generate_tests_arg7 = undefined;
//
function __generate_tests_wrapper() {
  var f = __generate_tests_func;
  if (typeof f !== "function") {
    throw Error("generate_tests: test function is not callable");
  }
  if (__generate_tests_arg_count === 0) {
    f();
    return;
  }
  if (__generate_tests_arg_count === 1) {
    f(__generate_tests_arg0);
    return;
  }
  if (__generate_tests_arg_count === 2) {
    f(__generate_tests_arg0, __generate_tests_arg1);
    return;
  }
  if (__generate_tests_arg_count === 3) {
    f(__generate_tests_arg0, __generate_tests_arg1, __generate_tests_arg2);
    return;
  }
  if (__generate_tests_arg_count === 4) {
    f(
      __generate_tests_arg0,
      __generate_tests_arg1,
      __generate_tests_arg2,
      __generate_tests_arg3
    );
    return;
  }
  if (__generate_tests_arg_count === 5) {
    f(
      __generate_tests_arg0,
      __generate_tests_arg1,
      __generate_tests_arg2,
      __generate_tests_arg3,
      __generate_tests_arg4
    );
    return;
  }
  if (__generate_tests_arg_count === 6) {
    f(
      __generate_tests_arg0,
      __generate_tests_arg1,
      __generate_tests_arg2,
      __generate_tests_arg3,
      __generate_tests_arg4,
      __generate_tests_arg5
    );
    return;
  }
  if (__generate_tests_arg_count === 7) {
    f(
      __generate_tests_arg0,
      __generate_tests_arg1,
      __generate_tests_arg2,
      __generate_tests_arg3,
      __generate_tests_arg4,
      __generate_tests_arg5,
      __generate_tests_arg6
    );
    return;
  }
  if (__generate_tests_arg_count === 8) {
    f(
      __generate_tests_arg0,
      __generate_tests_arg1,
      __generate_tests_arg2,
      __generate_tests_arg3,
      __generate_tests_arg4,
      __generate_tests_arg5,
      __generate_tests_arg6,
      __generate_tests_arg7
    );
    return;
  }
  throw Error("generate_tests: too many arguments");
}
//
function generate_tests(func, cases) {
  if (typeof func !== "function") {
    throw Error("generate_tests: test function is not callable");
  }
  if (!__is_array_like(cases)) {
    throw Error("generate_tests: cases must be array-like");
  }
  for (var i = 0; i < cases.length; i++) {
    var entry = cases[i];
    if (!__is_array_like(entry) || entry.length < 1) {
      continue;
    }
    var name = entry[0];
    __generate_tests_func = func;
    __generate_tests_arg_count = entry.length - 1;
    __generate_tests_arg0 = entry[1];
    __generate_tests_arg1 = entry[2];
    __generate_tests_arg2 = entry[3];
    __generate_tests_arg3 = entry[4];
    __generate_tests_arg4 = entry[5];
    __generate_tests_arg5 = entry[6];
    __generate_tests_arg6 = entry[7];
    __generate_tests_arg7 = entry[8];
    test(__generate_tests_wrapper, name);
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
  t.step = __async_test_step;
  t.step_func = __async_test_step_func;
  t.step_func_done = __async_test_step_func_done;
  t.step_timeout = __async_test_step_timeout;
  t.unreached_func = __async_test_unreached_func;
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
function __async_test_step(cb) {
  var t = this;
  if (!t || t._done === true) return;
  if (typeof cb !== "function") {
    __fail_test_record(t, Error("step: callback is not callable"));
    t.done();
    return;
  }
  try {
    cb();
  } catch (e) {
    __fail_test_record(t, e);
    t.done();
  }
}
//
function __async_test_step_timeout(cb, timeout_ms) {
  var t = this;
  if (!t || t._done === true) return 0;
  if (typeof cb !== "function") {
    __fail_test_record(t, Error("step_timeout: callback is not callable"));
    t.done();
    return 0;
  }
  if (typeof setTimeout !== "function") {
    // No timers; run synchronously.
    t.step(cb);
    return 0;
  }
  __step_timeout_test = t;
  __step_timeout_callback = cb;
  var delay = timeout_ms;
  if (typeof delay !== "number" || delay !== delay || delay < 0) {
    delay = 0;
  }
  return setTimeout(__async_test_step_timeout_wrapper, delay);
}
//
var __step_timeout_test = null;
var __step_timeout_callback = null;
//
function __async_test_step_timeout_wrapper(a0, a1, a2, a3) {
  var t = __step_timeout_test;
  if (!t || t._done === true) return;
  //
  if (typeof __step_timeout_callback !== "function") {
    __fail_test_record(t, Error("step_timeout: callback is not callable"));
    t.done();
    return;
  }
  t.step(__step_timeout_callback);
}
//
function __async_test_unreached_func(message) {
  __unreached_func_test = this;
  __unreached_func_message = message;
  return __async_test_unreached_func_wrapper;
}
//
var __unreached_func_test = null;
var __unreached_func_message = null;
//
function __async_test_unreached_func_wrapper(a0, a1, a2, a3) {
  var t = __unreached_func_test;
  if (!t || t._done === true) return;
  __fail_test_record(t, Error(__unreached_func_message || "unreached_func"));
  t.done();
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
