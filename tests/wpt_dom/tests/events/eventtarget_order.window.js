// META: script=/resources/testharness.js
//
// Curated EventTarget propagation checks using an explicit parent chain:
// `new EventTarget(parent)`.

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

// --- capture/target/bubble ordering ---
var order_step = 0;

function order_root_capture(_e) {
  if (failed) return;
  if (order_step !== 0) {
    fail("root capture ran out of order");
    return;
  }
  order_step = 1;
}

function order_parent_capture(_e) {
  if (failed) return;
  if (order_step !== 1) {
    fail("parent capture ran out of order");
    return;
  }
  order_step = 2;
}

function order_target_capture(_e) {
  if (failed) return;
  if (order_step !== 2) {
    fail("target capture ran out of order");
    return;
  }
  order_step = 3;
}

function order_target_bubble(_e) {
  if (failed) return;
  if (order_step !== 3) {
    fail("target bubble ran out of order");
    return;
  }
  order_step = 4;
}

function order_parent_bubble(_e) {
  if (failed) return;
  if (order_step !== 4) {
    fail("parent bubble ran out of order");
    return;
  }
  order_step = 5;
}

function order_root_bubble(_e) {
  if (failed) return;
  if (order_step !== 5) {
    fail("root bubble ran out of order");
    return;
  }
  order_step = 6;
}

var order_root = new EventTarget();
var order_parent = new EventTarget(order_root);
var order_target = new EventTarget(order_parent);

order_root.addEventListener("x", order_root_capture, { capture: true });
order_parent.addEventListener("x", order_parent_capture, { capture: true });
order_target.addEventListener("x", order_target_capture, { capture: true });
order_target.addEventListener("x", order_target_bubble);
order_parent.addEventListener("x", order_parent_bubble);
order_root.addEventListener("x", order_root_bubble);

order_target.dispatchEvent(new Event("x", { bubbles: true }));
if (!failed) {
  if (order_step !== 6) {
    fail("expected capture/target/bubble listeners to all run");
  }
}

// --- stopPropagation ---
var stop_parent_ran = false;
var stop_root_ran = false;

function stop_parent_listener(e) {
  stop_parent_ran = true;
  e.stopPropagation();
}

function stop_root_listener(_e) {
  stop_root_ran = true;
}

if (!failed) {
  var stop_root = new EventTarget();
  var stop_parent = new EventTarget(stop_root);
  var stop_target = new EventTarget(stop_parent);

  stop_parent.addEventListener("x", stop_parent_listener);
  stop_root.addEventListener("x", stop_root_listener);
  stop_target.dispatchEvent(new Event("x", { bubbles: true }));

  if (stop_parent_ran !== true) {
    fail("parent listener did not run");
  } else if (stop_root_ran !== false) {
    fail("stopPropagation should prevent dispatch to root");
  }
}

// --- stopImmediatePropagation ---
var immediate_first_ran = false;
var immediate_second_ran = false;
var immediate_parent_ran = false;

function immediate_first_listener(e) {
  immediate_first_ran = true;
  e.stopImmediatePropagation();
}

function immediate_second_listener(_e) {
  immediate_second_ran = true;
}

function immediate_parent_listener(_e) {
  immediate_parent_ran = true;
}

if (!failed) {
  var immediate_root = new EventTarget();
  var immediate_parent = new EventTarget(immediate_root);
  var immediate_target = new EventTarget(immediate_parent);

  immediate_target.addEventListener("x", immediate_first_listener);
  immediate_target.addEventListener("x", immediate_second_listener);
  immediate_parent.addEventListener("x", immediate_parent_listener);

  immediate_target.dispatchEvent(new Event("x", { bubbles: true }));

  if (immediate_first_ran !== true) {
    fail("first listener did not run");
  } else if (immediate_second_ran !== false) {
    fail("stopImmediatePropagation should stop other listeners on the same target");
  } else if (immediate_parent_ran !== false) {
    fail("stopImmediatePropagation should stop propagation to parents");
  }
}

if (!failed) {
  report_pass();
}
