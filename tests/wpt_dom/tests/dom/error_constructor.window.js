// META: script=/resources/testharness.js
//
// Basic sanity: `Error` should exist and be constructible so tests can throw failures using
// `new Error(...)`.
//
// This is intentionally tiny and reports directly via `__fastrender_wpt_report` so it can run on
// the minimal vm-js WPT backend without relying on the full `testharness.js` assertion surface.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var failed = false;

function fail(message) {
  if (failed) return;
  failed = true;
  report_fail(message);
}

var err = null;
try {
  err = new Error("boom");
} catch (e) {
  fail("new Error(...) should not throw");
}

if (!failed) {
  if (err === null) {
    fail("new Error(...) should return an object");
  } else if (err.name !== "Error") {
    fail("Error instance name should be 'Error'");
  } else if (err.message !== "boom") {
    fail("Error instance message should match constructor argument");
  }
}

if (!failed) {
  var caught = null;
  try {
    throw err;
  } catch (e) {
    caught = e;
  }
  if (caught !== err) {
    fail("throw/catch should preserve the thrown Error object");
  }
}

if (!failed) {
  report_pass();
}
