/*
 * Minimal `testharness.js` subset for FastRender's offline WPT DOM smoke tests.
 *
 * This is intentionally not a verbatim copy of upstream WPT. It implements only the
 * tiny surface area needed by `tests/wpt_dom/tests/smoke/`:
 *   - test()
 *   - async_test()
 *   - promise_test()
 *   - assert_true/assert_false/assert_equals/assert_unreached
 *   - add_result_callback/add_completion_callback
 *   - basic uncaught exception / unhandledrejection -> harness error reporting
 *
 * When the `cargo xtask js wpt-dom` runner is implemented, prefer swapping this file
 * with a pinned upstream WPT copy if possible.
 */

(function (global) {
  "use strict";

  const PASS = 0;
  const FAIL = 1;
  const TIMEOUT = 2;
  const NOTRUN = 3;

  global.PASS = PASS;
  global.FAIL = FAIL;
  global.TIMEOUT = TIMEOUT;
  global.NOTRUN = NOTRUN;

  class AssertionError extends Error {
    constructor(message) {
      super(message || "Assertion failed");
      this.name = "AssertionError";
    }
  }

  function stackOf(err) {
    if (err && typeof err === "object" && "stack" in err && err.stack) {
      return String(err.stack);
    }
    return null;
  }

  function stringifyError(err) {
    if (err instanceof Error) {
      return err.message || String(err);
    }
    return String(err);
  }

  function assert_true(value, message) {
    if (value !== true) {
      throw new AssertionError(
        message || `assert_true: expected true but got ${String(value)}`,
      );
    }
  }

  function assert_false(value, message) {
    if (value !== false) {
      throw new AssertionError(
        message || `assert_false: expected false but got ${String(value)}`,
      );
    }
  }

  function assert_equals(actual, expected, message) {
    if (actual !== expected) {
      throw new AssertionError(
        message ||
          `assert_equals: expected ${String(expected)} but got ${String(actual)}`,
      );
    }
  }

  function assert_unreached(message) {
    throw new AssertionError(message || "assert_unreached");
  }

  class Test {
    constructor(name) {
      this.name = name || "(unnamed test)";
      this.status = NOTRUN;
      this.message = null;
      this.stack = null;
    }

    pass() {
      this.status = PASS;
      this.message = null;
      this.stack = null;
    }

    fail(err) {
      this.status = FAIL;
      this.message = stringifyError(err);
      this.stack = stackOf(err);
    }

    timeout(message) {
      this.status = TIMEOUT;
      this.message = message || "timeout";
      this.stack = null;
    }
  }

  const tests = [];
  let pending = 0;
  let completed = false;

  const resultCallbacks = [];
  const completionCallbacks = [];

  const harnessStatus = {
    status: 0,
    message: null,
    stack: null,
  };

  function fireResult(t) {
    for (const cb of resultCallbacks) {
      try {
        cb(t);
      } catch (_err) {
        // Ignore reporter errors.
      }
    }
  }

  function fireCompletion() {
    if (completed) return;
    completed = true;
    const testsCopy = tests.slice();
    const statusCopy = { ...harnessStatus };
    for (const cb of completionCallbacks) {
      try {
        cb(testsCopy, statusCopy);
      } catch (_err) {
        // Ignore reporter errors.
      }
    }
  }

  function scheduleCompletionCheck() {
    if (completed) return;
    if (pending !== 0) return;

    const schedule =
      typeof global.queueMicrotask === "function"
        ? global.queueMicrotask.bind(global)
        : (fn) => setTimeout(fn, 0);

    schedule(() => {
      if (completed) return;
      if (pending === 0) fireCompletion();
    });
  }

  function test(fn, name) {
    const t = new Test(name);
    tests.push(t);
    try {
      fn();
      t.pass();
    } catch (err) {
      t.fail(err);
    }
    fireResult(t);
    scheduleCompletionCheck();
    return t;
  }

  class AsyncTest extends Test {
    constructor(name, timeoutMs) {
      super(name);
      this._done = false;
      this._timeoutMs = timeoutMs || 1000;

      pending += 1;
      this._timer = setTimeout(() => {
        if (this._done) return;
        this.timeout(`timeout after ${this._timeoutMs}ms`);
        this.done();
      }, this._timeoutMs);
    }

    step_func(fn) {
      return (...args) => {
        if (this._done) return;
        try {
          return fn.apply(this, args);
        } catch (err) {
          this.fail(err);
          this.done();
        }
      };
    }

    step_func_done(fn) {
      return this.step_func((...args) => {
        if (fn) fn.apply(this, args);
        this.done();
      });
    }

    done() {
      if (this._done) return;
      this._done = true;
      clearTimeout(this._timer);

      if (this.status === NOTRUN) {
        this.pass();
      }

      pending -= 1;
      fireResult(this);
      scheduleCompletionCheck();
    }
  }

  function async_test(fn, name) {
    const t = new AsyncTest(name);
    tests.push(t);
    try {
      fn(t);
    } catch (err) {
      t.fail(err);
      t.done();
    }
    return t;
  }

  function promise_test(fn, name) {
    const t = new AsyncTest(name);
    tests.push(t);

    let p;
    try {
      p = fn(t);
    } catch (err) {
      t.fail(err);
      t.done();
      return t;
    }

    Promise.resolve(p).then(
      () => t.done(),
      (err) => {
        t.fail(err);
        t.done();
      },
    );

    return t;
  }

  function add_result_callback(fn) {
    resultCallbacks.push(fn);
  }

  function add_completion_callback(fn) {
    completionCallbacks.push(fn);
    // If all tests already finished, ensure we still notify newly added reporters.
    scheduleCompletionCheck();
  }

  function setHarnessError(err) {
    if (harnessStatus.status !== 0) return;

    harnessStatus.status = 1;
    harnessStatus.message = stringifyError(err);
    harnessStatus.stack = stackOf(err);

    // Treat a harness-level error as terminal; the runner can decide how to interpret it.
    fireCompletion();
  }

  if (typeof global.addEventListener === "function") {
    global.addEventListener("error", (event) => {
      setHarnessError(event && event.error ? event.error : event);
    });
    global.addEventListener("unhandledrejection", (event) => {
      setHarnessError(event && "reason" in event ? event.reason : event);
    });
  } else {
    // Best-effort fallback for non-DOM environments.
    const prev = global.onerror;
    global.onerror = function (_message, _source, _lineno, _colno, error) {
      setHarnessError(error || _message);
      if (typeof prev === "function") {
        return prev.apply(this, arguments);
      }
      return false;
    };
  }

  global.AssertionError = AssertionError;
  global.test = test;
  global.async_test = async_test;
  global.promise_test = promise_test;
  global.assert_true = assert_true;
  global.assert_false = assert_false;
  global.assert_equals = assert_equals;
  global.assert_unreached = assert_unreached;
  global.add_result_callback = add_result_callback;
  global.add_completion_callback = add_completion_callback;
})(typeof globalThis !== "undefined" ? globalThis : this);

