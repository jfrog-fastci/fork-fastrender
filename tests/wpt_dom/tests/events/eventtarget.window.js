// META: script=/resources/testharness.js

// Curated DOM EventTarget semantics checks. These tests use the host-provided
// `EventTarget`/`Event`/DOM shims and report directly via `__fastrender_wpt_report`.

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

// --- capture/bubble ordering (Window -> Document -> parent -> child) ---
var order_step = 0;

function parent_capture(_e) {
  if (failed) return;
  if (order_step !== 0) {
    fail("parent capture ran out of order");
    return;
  }
  order_step = 1;
}

function child_capture(_e) {
  if (failed) return;
  if (order_step !== 1) {
    fail("child capture ran out of order");
    return;
  }
  order_step = 2;
}

function child_bubble(_e) {
  if (failed) return;
  if (order_step !== 2) {
    fail("child bubble ran out of order");
    return;
  }
  order_step = 3;
}

function parent_bubble(_e) {
  if (failed) return;
  if (order_step !== 3) {
    fail("parent bubble ran out of order");
    return;
  }
  order_step = 4;
}

if (!failed) {
  var parent = document.createElement("div");
  var child = document.createElement("span");

  // Attach the subtree to the document so the event path is:
  // Window -> Document -> parent -> child.
  document.appendChild(parent);
  parent.appendChild(child);

  parent.addEventListener("x", parent_capture, true);
  parent.addEventListener("x", parent_bubble);

  child.addEventListener("x", child_capture, { capture: true });
  child.addEventListener("x", child_bubble);

  var ev = new Event("x", { bubbles: true });
  var ok = child.dispatchEvent(ev);
  if (ok !== true) {
    fail("dispatchEvent should return true when not canceled");
  } else if (order_step !== 4) {
    fail("expected capture listeners to run before bubbling listeners");
  }
}

// --- preventDefault + cancelable ---
var prevent_saw = false;

function prevent_listener(e) {
  prevent_saw = true;
  e.preventDefault();
}

if (!failed) {
  var el = document.createElement("div");
  el.addEventListener("x", prevent_listener);

  var ev2 = new Event("x", { cancelable: true });
  var ok2 = el.dispatchEvent(ev2);

  if (prevent_saw !== true) {
    fail("listener should have run");
  } else if (ev2.defaultPrevented !== true) {
    fail("preventDefault should set defaultPrevented for cancelable events");
  } else if (ok2 !== false) {
    fail("dispatchEvent should return false when default was prevented");
  }
}

// --- removeEventListener ---
var removed_called = false;

function removed_cb(_e) {
  removed_called = true;
}

if (!failed) {
  var el2 = document.createElement("div");
  el2.addEventListener("x", removed_cb);
  el2.removeEventListener("x", removed_cb);
  el2.dispatchEvent(new Event("x"));

  if (removed_called !== false) {
    fail("removed listener should not be invoked");
  }
}

// --- addEventListener ignores duplicates ---
var dup_called = 0;

function dup_cb(_e) {
  if (dup_called === 0) {
    dup_called = 1;
  } else if (dup_called === 1) {
    dup_called = 2;
  } else {
    dup_called = 3;
  }
}

if (!failed) {
  var el3 = document.createElement("div");
  el3.addEventListener("x", dup_cb);
  el3.addEventListener("x", dup_cb);
  el3.dispatchEvent(new Event("x"));

  if (dup_called !== 1) {
    fail("duplicate addEventListener registrations should be ignored");
  }
}

if (!failed) {
  report_pass();
}
