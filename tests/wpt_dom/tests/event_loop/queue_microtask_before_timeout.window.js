// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: microtasks (queueMicrotask) must run before timers.

var microtask_ran = false;

function queue_microtask_and_timeout_apis_exist_test() {
  queueMicrotask;
  setTimeout;
}

test(queue_microtask_and_timeout_apis_exist_test, "queueMicrotask and setTimeout APIs exist");

function on_microtask() {
  microtask_ran = true;
}

function check_microtask_before_timeout() {
  if (microtask_ran !== true) {
    throw "queueMicrotask did not run before setTimeout";
  }
}

function run_queue_microtask_before_timeout_test(t) {
  microtask_ran = false;
  queueMicrotask(on_microtask);
  setTimeout(t.step_func_done(check_microtask_before_timeout), 0);
}

async_test(
  run_queue_microtask_before_timeout_test,
  "queueMicrotask microtasks run before timers"
);
