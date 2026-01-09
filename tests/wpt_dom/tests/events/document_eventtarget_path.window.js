// META: script=/resources/testharness.js
//
// Curated DOM EventTarget propagation checks using the host `document` + `createElement` shims.
// This avoids JS features not yet supported by the vm-js WPT runner (arrays, arrow functions, etc.)
// while still validating capture/target/bubble ordering through a document-attached element tree.

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

var parent = document.createElement("div");
var child = document.createElement("span");

// Attach the subtree to the document so the event path includes:
// Window -> Document -> parent -> child.
document.appendChild(parent);
parent.appendChild(child);

parent.addEventListener("x", parent_capture, true);
child.addEventListener("x", child_capture, true);
child.addEventListener("x", child_bubble);
parent.addEventListener("x", parent_bubble);

child.dispatchEvent(new Event("x", { bubbles: true }));

if (!failed) {
  if (order_step !== 4) {
    fail("expected all capture/target/bubble listeners to run");
  }
}

if (!failed) {
  report_pass();
}

