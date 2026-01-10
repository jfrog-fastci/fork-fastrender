// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: multiple microtasks should execute in FIFO order, and the
// microtask checkpoint must run before timers.

var step = 0;
var error = "";

function queue_microtask_api_exists_test() {
  queueMicrotask;
}

test(queue_microtask_api_exists_test, "queueMicrotask API exists");

function microtask_first() {
  if (step !== 0) {
    if (error === "") error = "first microtask ran out of order";
    return;
  }
  step = 1;
}

function microtask_second() {
  if (step !== 1) {
    if (error === "") error = "second microtask ran out of order";
    return;
  }
  step = 2;
}

function check_microtask_order_and_checkpoint() {
  if (error !== "") {
    throw error;
  }
  if (step !== 2) {
    throw "microtasks did not complete before setTimeout";
  }
}

function run_queue_microtask_order_test(t) {
  step = 0;
  error = "";

  queueMicrotask(microtask_first);
  queueMicrotask(microtask_second);
  setTimeout(t.step_func_done(check_microtask_order_and_checkpoint), 0);
}

async_test(run_queue_microtask_order_test, "queueMicrotask runs FIFO and completes before timers");
