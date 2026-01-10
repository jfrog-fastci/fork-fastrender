// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: Promise reactions run in the microtask queue and must be
// processed before timers.

var then_ran = false;

function promise_api_exists_test() {
  Promise.resolve(1);
}

test(promise_api_exists_test, "Promise API exists");

function on_then(_value) {
  then_ran = true;
}

function check_then_before_timeout() {
  if (then_ran !== true) {
    throw "Promise.then did not run before setTimeout";
  }
}

function run_then_before_timeout_test(t) {
  then_ran = false;
  Promise.resolve(1).then(on_then);
  setTimeout(t.step_func_done(check_then_before_timeout), 0);
}

async_test(run_then_before_timeout_test, "Promise.then microtasks run before timers");
