// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Regression test: the vm-js WPT backend must use deterministic virtual time.
// `Date.now()` and `performance.now()` should both start at 0 and advance with the virtual clock
// when timers fire (without wall-clock sleeps).
//
// Note: we schedule a long timer (1000ms) after the initial 10ms timer to ensure the backend uses
// `idle_wait` fast-forward semantics rather than waiting in real time.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

// Ensure the runner exposes spec-shaped globals as both identifiers and global properties.
if (typeof globalThis === "undefined") {
  report_fail("globalThis should exist");
} else if (typeof globalThis.Date === "undefined") {
  report_fail("globalThis.Date should exist");
} else if (typeof Date === "undefined") {
  report_fail("Date identifier should exist");
} else if (Date !== globalThis.Date) {
  report_fail("Date identifier should match globalThis.Date");
} else if (typeof globalThis.Date.now !== "function") {
  report_fail("globalThis.Date.now should be a function");
} else if (typeof globalThis.performance === "undefined") {
  report_fail("globalThis.performance should exist");
} else if (typeof performance === "undefined") {
  report_fail("performance identifier should exist");
} else if (performance !== globalThis.performance) {
  report_fail("performance identifier should match globalThis.performance");
} else if (typeof globalThis.performance.now !== "function") {
  report_fail("globalThis.performance.now should be a function");
} else if (typeof globalThis.setTimeout !== "function") {
  report_fail("globalThis.setTimeout should be a function");
} else if (typeof setTimeout === "undefined") {
  report_fail("setTimeout identifier should exist");
} else if (setTimeout !== globalThis.setTimeout) {
  report_fail("setTimeout identifier should match globalThis.setTimeout");
} else {
  var start_date = globalThis.Date.now();
  var start_perf = globalThis.performance.now();

  if (start_date !== 0) {
    report_fail("Date.now() should start at 0");
  } else if (start_perf !== 0) {
    report_fail("performance.now() should start at 0");
  } else if (start_perf !== start_date) {
    report_fail("performance.now() should match Date.now() at start");
  } else {
    globalThis.setTimeout(on_timeout_10, 10);
  }
}

function on_timeout_1010() {
  var now_date = globalThis.Date.now();
  var now_perf = globalThis.performance.now();

  if (now_date !== 1010) {
    report_fail("Date.now() should be 1010 in long setTimeout callback");
    return;
  }
  if (now_perf !== 1010) {
    report_fail("performance.now() should be 1010 in long setTimeout callback");
    return;
  }
  if (now_perf !== now_date) {
    report_fail("performance.now() should match Date.now() in long setTimeout callback");
    return;
  }

  report_pass();
}

function on_timeout_10() {
  var now_date = globalThis.Date.now();
  var now_perf = globalThis.performance.now();

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

  globalThis.setTimeout(on_timeout_1010, 1000);
}
