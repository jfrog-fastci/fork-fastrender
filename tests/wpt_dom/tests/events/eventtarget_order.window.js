// META: script=/resources/testharness.js
//
// Curated EventTarget propagation checks using an explicit parent chain:
// `new EventTarget(parent)`.

test(() => {
  var order_step = 0;

  function order_root_capture(_e) {
    assert_equals(order_step, 0, "root capture ran out of order");
    order_step = 1;
  }

  function order_parent_capture(_e) {
    assert_equals(order_step, 1, "parent capture ran out of order");
    order_step = 2;
  }

  function order_target_capture(_e) {
    assert_equals(order_step, 2, "target capture ran out of order");
    order_step = 3;
  }

  function order_target_bubble(_e) {
    assert_equals(order_step, 3, "target bubble ran out of order");
    order_step = 4;
  }

  function order_parent_bubble(_e) {
    assert_equals(order_step, 4, "parent bubble ran out of order");
    order_step = 5;
  }

  function order_root_bubble(_e) {
    assert_equals(order_step, 5, "root bubble ran out of order");
    order_step = 6;
  }

  var root = new EventTarget();
  var parent = new EventTarget(root);
  var target = new EventTarget(parent);

  root.addEventListener("order", order_root_capture, { capture: true });
  parent.addEventListener("order", order_parent_capture, { capture: true });
  target.addEventListener("order", order_target_capture, { capture: true });
  target.addEventListener("order", order_target_bubble);
  parent.addEventListener("order", order_parent_bubble);
  root.addEventListener("order", order_root_bubble);

  target.dispatchEvent(new Event("order", { bubbles: true }));
  assert_equals(order_step, 6, "expected capture/target/bubble listeners to all run");
}, "capture/target/bubble ordering through an explicit EventTarget parent chain");

test(() => {
  var parent_ran = false;
  var root_ran = false;

  function stop_parent_listener(e) {
    parent_ran = true;
    e.stopPropagation();
  }

  function stop_root_listener(_e) {
    root_ran = true;
  }

  var root = new EventTarget();
  var parent = new EventTarget(root);
  var target = new EventTarget(parent);

  parent.addEventListener("stop-propagation", stop_parent_listener);
  root.addEventListener("stop-propagation", stop_root_listener);
  target.dispatchEvent(new Event("stop-propagation", { bubbles: true }));

  assert_true(parent_ran, "parent listener did not run");
  assert_false(root_ran, "stopPropagation should prevent dispatch to root");
}, "stopPropagation stops propagation to ancestors");

test(() => {
  var first_ran = false;
  var second_ran = false;
  var parent_ran = false;

  function immediate_first_listener(e) {
    first_ran = true;
    e.stopImmediatePropagation();
  }

  function immediate_second_listener(_e) {
    second_ran = true;
  }

  function immediate_parent_listener(_e) {
    parent_ran = true;
  }

  var root = new EventTarget();
  var parent = new EventTarget(root);
  var target = new EventTarget(parent);

  target.addEventListener("stop-immediate", immediate_first_listener);
  target.addEventListener("stop-immediate", immediate_second_listener);
  parent.addEventListener("stop-immediate", immediate_parent_listener);

  target.dispatchEvent(new Event("stop-immediate", { bubbles: true }));

  assert_true(first_ran, "first listener did not run");
  assert_false(second_ran, "stopImmediatePropagation should stop other listeners on the same target");
  assert_false(parent_ran, "stopImmediatePropagation should stop propagation to parents");
}, "stopImmediatePropagation stops remaining listeners on the current target and stops propagation");
