// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: Promise reactions run in the microtask queue and must be
// processed before timers.

var then_ran = false;

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function on_then(_value) {
  then_ran = true;
}

function on_timeout() {
  if (then_ran !== true) {
    report_fail("Promise.then did not run before setTimeout");
    return;
  }
  report_pass();
}

Promise.resolve(1).then(on_then);
setTimeout(on_timeout, 0);
