// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop test: `setTimeout` should pass additional arguments to the callback.

var timeout_args_obj = { ok: true };
var timeout_args_a;
var timeout_args_b;
var timeout_args_c;
var timeout_args_finish;

function settimeout_api_exists_test() {
  setTimeout;
}

test(settimeout_api_exists_test, "setTimeout API exists");

function check_settimeout_args() {
  if (timeout_args_a !== 1) {
    throw "expected first arg to be 1";
  }
  if (timeout_args_b !== "two") {
    throw "expected second arg to be 'two'";
  }
  if (timeout_args_c !== timeout_args_obj) {
    throw "expected third arg to match object identity";
  }
}

function on_settimeout_args_timeout(a, b, c) {
  timeout_args_a = a;
  timeout_args_b = b;
  timeout_args_c = c;
  timeout_args_finish();
}

function run_settimeout_args_test(t) {
  timeout_args_a = 0;
  timeout_args_b = "";
  timeout_args_c = 0;
  timeout_args_finish = t.step_func_done(check_settimeout_args);
  setTimeout(on_settimeout_args_timeout, 0, 1, "two", timeout_args_obj);
}

async_test(run_settimeout_args_test, "setTimeout passes additional callback arguments");
