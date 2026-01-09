// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: microtasks (queueMicrotask) must run before timers.

var microtask_ran = false;

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function on_microtask() {
  microtask_ran = true;
}

function on_timeout() {
  if (microtask_ran !== true) {
    report_fail("queueMicrotask did not run before setTimeout");
    return;
  }
  report_pass();
}

queueMicrotask(on_microtask);
setTimeout(on_timeout, 0);
