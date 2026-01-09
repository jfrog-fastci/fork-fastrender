// META: script=/resources/testharness.js
//
// Curated EventTarget semantics checks. These tests use the host-provided `EventTarget`/`Event`
// implementations and report via `__fastrender_wpt_report`.

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

// --- dispatch order at target ---
var dispatch_step = 0;

function dispatch_capture(_e) {
  if (failed) return;
  if (dispatch_step !== 0) {
    fail("capture listener ran out of order");
    return;
  }
  dispatch_step = 1;
}

function dispatch_bubble(_e) {
  if (failed) return;
  if (dispatch_step !== 1) {
    fail("bubble listener ran out of order");
    return;
  }
  dispatch_step = 2;
}

var dispatch_target = new EventTarget();
dispatch_target.addEventListener("x", dispatch_capture, { capture: true });
dispatch_target.addEventListener("x", dispatch_bubble);
dispatch_target.dispatchEvent(new Event("x"));
if (!failed) {
  if (dispatch_step !== 2) {
    fail("expected both capture and bubble listeners to run");
  }
}

// --- removeEventListener ---
var removed_called = false;

function removed_cb(_e) {
  removed_called = true;
}

if (!failed) {
  var remove_target = new EventTarget();
  remove_target.addEventListener("x", removed_cb);
  remove_target.removeEventListener("x", removed_cb);
  remove_target.dispatchEvent(new Event("x"));
  if (removed_called !== false) {
    fail("removed listener should not be invoked");
  }
}

// --- once ---
var once_called = false;

function once_cb(_e) {
  if (failed) return;
  if (once_called) {
    fail("once listener ran more than once");
    return;
  }
  once_called = true;
}

if (!failed) {
  var once_target = new EventTarget();
  once_target.addEventListener("x", once_cb, { once: true });
  once_target.dispatchEvent(new Event("x"));
  once_target.dispatchEvent(new Event("x"));
  if (once_called !== true) {
    fail("once listener did not run");
  }
}

// --- passive ---
var passive_called = false;
var passive_event = null;

function passive_cb(e) {
  passive_called = true;
  passive_event = e;
  e.preventDefault();
}

if (!failed) {
  var passive_target = new EventTarget();
  passive_target.addEventListener("x", passive_cb, { passive: true });
  var ev = new Event("x", { cancelable: true });
  var ok = passive_target.dispatchEvent(ev);
  if (passive_called !== true) {
    fail("passive listener was not invoked");
  } else if (ok !== true) {
    fail("dispatchEvent should return true when preventDefault was ignored");
  } else if (ev.defaultPrevented !== false) {
    fail("preventDefault() in passive listener must be ignored");
  } else if (passive_event !== ev) {
    fail("listener should receive the same Event object passed to dispatchEvent");
  }
}

if (!failed) {
  report_pass();
}
