// META: script=/resources/testharness.js
//
// Curated EventTarget propagation checks using an explicit parent chain:
// `new EventTarget(parent)`.

// --- capture/target/bubble ordering ---
var eventtarget_order_window_order_step = 0;

function eventtarget_order_window_order_root_capture(_e) {
  assert_equals(eventtarget_order_window_order_step, 0, "root capture ran out of order");
  eventtarget_order_window_order_step = 1;
}

function eventtarget_order_window_order_parent_capture(_e) {
  assert_equals(eventtarget_order_window_order_step, 1, "parent capture ran out of order");
  eventtarget_order_window_order_step = 2;
}

function eventtarget_order_window_order_target_capture(_e) {
  assert_equals(eventtarget_order_window_order_step, 2, "target capture ran out of order");
  eventtarget_order_window_order_step = 3;
}

function eventtarget_order_window_order_target_bubble(_e) {
  assert_equals(eventtarget_order_window_order_step, 3, "target bubble ran out of order");
  eventtarget_order_window_order_step = 4;
}

function eventtarget_order_window_order_parent_bubble(_e) {
  assert_equals(eventtarget_order_window_order_step, 4, "parent bubble ran out of order");
  eventtarget_order_window_order_step = 5;
}

function eventtarget_order_window_order_root_bubble(_e) {
  assert_equals(eventtarget_order_window_order_step, 5, "root bubble ran out of order");
  eventtarget_order_window_order_step = 6;
}

function eventtarget_order_window_capture_target_bubble_order_test() {
  eventtarget_order_window_order_step = 0;

  var order_root = new EventTarget();
  var order_parent = new EventTarget(order_root);
  var order_target = new EventTarget(order_parent);

  order_root.addEventListener("order", eventtarget_order_window_order_root_capture, {
    capture: true,
  });
  order_parent.addEventListener("order", eventtarget_order_window_order_parent_capture, {
    capture: true,
  });
  order_target.addEventListener("order", eventtarget_order_window_order_target_capture, {
    capture: true,
  });
  order_target.addEventListener("order", eventtarget_order_window_order_target_bubble);
  order_parent.addEventListener("order", eventtarget_order_window_order_parent_bubble);
  order_root.addEventListener("order", eventtarget_order_window_order_root_bubble);

  order_target.dispatchEvent(new Event("order", { bubbles: true }));

  assert_equals(
    eventtarget_order_window_order_step,
    6,
    "expected capture/target/bubble listeners to all run"
  );
}

test(
  eventtarget_order_window_capture_target_bubble_order_test,
  "EventTarget propagation runs capture, then at-target, then bubble"
);

// --- stopPropagation ---
var eventtarget_order_window_stop_parent_ran = false;
var eventtarget_order_window_stop_root_ran = false;

function eventtarget_order_window_stop_parent_listener(e) {
  eventtarget_order_window_stop_parent_ran = true;
  e.stopPropagation();
}

function eventtarget_order_window_stop_root_listener(_e) {
  eventtarget_order_window_stop_root_ran = true;
}

function eventtarget_order_window_stop_propagation_test() {
  eventtarget_order_window_stop_parent_ran = false;
  eventtarget_order_window_stop_root_ran = false;

  var stop_root = new EventTarget();
  var stop_parent = new EventTarget(stop_root);
  var stop_target = new EventTarget(stop_parent);

  stop_parent.addEventListener("stop-propagation", eventtarget_order_window_stop_parent_listener);
  stop_root.addEventListener("stop-propagation", eventtarget_order_window_stop_root_listener);
  stop_target.dispatchEvent(new Event("stop-propagation", { bubbles: true }));

  assert_true(eventtarget_order_window_stop_parent_ran, "parent listener did not run");
  assert_false(
    eventtarget_order_window_stop_root_ran,
    "stopPropagation should prevent dispatch to root"
  );
}

test(
  eventtarget_order_window_stop_propagation_test,
  "stopPropagation stops dispatch to further ancestors"
);

// --- stopImmediatePropagation ---
var eventtarget_order_window_immediate_first_ran = false;
var eventtarget_order_window_immediate_second_ran = false;
var eventtarget_order_window_immediate_parent_ran = false;

function eventtarget_order_window_immediate_first_listener(e) {
  eventtarget_order_window_immediate_first_ran = true;
  e.stopImmediatePropagation();
}

function eventtarget_order_window_immediate_second_listener(_e) {
  eventtarget_order_window_immediate_second_ran = true;
}

function eventtarget_order_window_immediate_parent_listener(_e) {
  eventtarget_order_window_immediate_parent_ran = true;
}

function eventtarget_order_window_stop_immediate_propagation_test() {
  eventtarget_order_window_immediate_first_ran = false;
  eventtarget_order_window_immediate_second_ran = false;
  eventtarget_order_window_immediate_parent_ran = false;

  var immediate_root = new EventTarget();
  var immediate_parent = new EventTarget(immediate_root);
  var immediate_target = new EventTarget(immediate_parent);

  immediate_target.addEventListener("stop-immediate", eventtarget_order_window_immediate_first_listener);
  immediate_target.addEventListener("stop-immediate", eventtarget_order_window_immediate_second_listener);
  immediate_parent.addEventListener("stop-immediate", eventtarget_order_window_immediate_parent_listener);

  immediate_target.dispatchEvent(new Event("stop-immediate", { bubbles: true }));

  assert_true(eventtarget_order_window_immediate_first_ran, "first listener did not run");
  assert_false(
    eventtarget_order_window_immediate_second_ran,
    "stopImmediatePropagation should stop other listeners on the same target"
  );
  assert_false(
    eventtarget_order_window_immediate_parent_ran,
    "stopImmediatePropagation should stop propagation to parents"
  );
}

test(
  eventtarget_order_window_stop_immediate_propagation_test,
  "stopImmediatePropagation stops other listeners and stops propagation to ancestors"
);
