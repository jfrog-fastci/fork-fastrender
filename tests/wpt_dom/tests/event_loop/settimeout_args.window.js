// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop test: `setTimeout` should pass additional arguments to the callback.

var obj = { ok: true };

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

function done(a, b, c) {
  if (a !== 1) {
    report_fail("expected first arg to be 1");
    return;
  }
  if (b !== "two") {
    report_fail("expected second arg to be 'two'");
    return;
  }
  if (c !== obj) {
    report_fail("expected third arg to match object identity");
    return;
  }
  report_pass();
}

setTimeout(done, 0, 1, "two", obj);
