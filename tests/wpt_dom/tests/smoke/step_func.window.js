// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Smoke test: `async_test` should expose `t.step_func`, and the wrapper should be usable as a
// `setTimeout` callback.
//
// Keep state in globals so timer callbacks do not capture locals (for compatibility with the
// minimal vm-js backend).
var step_func_t = null;
var step_func_wrapped = null;
var step_func_called = false;

function on_step_func_timeout() {
  assert_false(step_func_called, "step_func callback should run exactly once");
  step_func_called = true;
  step_func_t.done();
}

function run_step_func_test(t) {
  step_func_t = t;
  step_func_called = false;

  assert_equals(
    typeof t.step_func,
    "function",
    "async_test should provide t.step_func"
  );
  step_func_wrapped = t.step_func(on_step_func_timeout);
  assert_equals(
    typeof step_func_wrapped,
    "function",
    "t.step_func should return a function wrapper"
  );

  setTimeout(step_func_wrapped, 0);
}

async_test(run_step_func_test, "t.step_func wraps async callbacks");
