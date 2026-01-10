// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Regression test: the vm-js WPT backend must use deterministic virtual time.
// `Date.now()` and `performance.now()` should both start at 0 and advance with the virtual clock
// when timers fire (without wall-clock sleeps).

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var start_date = Date.now();
var start_perf = performance.now();

function on_timeout() {
  var now_date = Date.now();
  var now_perf = performance.now();

  if (now_date !== 10) {
    report_fail("Date.now() should be 10 in setTimeout callback");
    return;
  }
  if (now_perf !== 10) {
    report_fail("performance.now() should be 10 in setTimeout callback");
    return;
  }
  if (now_perf !== now_date) {
    report_fail("performance.now() should match Date.now() in setTimeout callback");
    return;
  }

  report_pass();
}

if (start_date !== 0) {
  report_fail("Date.now() should start at 0");
} else if (start_perf !== 0) {
  report_fail("performance.now() should start at 0");
} else if (start_perf !== start_date) {
  report_fail("performance.now() should match Date.now() at start");
} else {
  setTimeout(on_timeout, 10);
}
