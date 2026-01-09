// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: multiple microtasks should execute in FIFO order, and the
// microtask checkpoint must run before timers.

var step = 0;

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function microtask_first() {
  if (step !== 0) {
    report_fail("first microtask ran out of order");
    return;
  }
  step = 1;
}

function microtask_second() {
  if (step !== 1) {
    report_fail("second microtask ran out of order");
    return;
  }
  step = 2;
}

function on_timeout() {
  if (step !== 2) {
    report_fail("microtasks did not complete before setTimeout");
    return;
  }
  report_pass();
}

queueMicrotask(microtask_first);
queueMicrotask(microtask_second);
setTimeout(on_timeout, 0);

