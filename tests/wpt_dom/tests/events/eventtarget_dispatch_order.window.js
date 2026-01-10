// META: script=/resources/testharness.js
//
// Curated EventTarget dispatch semantics checks. This file intentionally uses conservative JS
// syntax so it can run on the minimal vm-js backend.

// --- dispatch order at target ---
var eventtarget_dispatch_order_dispatch_step = 0;

function eventtarget_dispatch_order_dispatch_capture(_e) {
  assert_equals(
    eventtarget_dispatch_order_dispatch_step,
    0,
    "capture listener ran out of order"
  );
  eventtarget_dispatch_order_dispatch_step = 1;
}

function eventtarget_dispatch_order_dispatch_bubble(_e) {
  assert_equals(
    eventtarget_dispatch_order_dispatch_step,
    1,
    "bubble listener ran out of order"
  );
  eventtarget_dispatch_order_dispatch_step = 2;
}

function eventtarget_dispatch_order_at_target_test() {
  eventtarget_dispatch_order_dispatch_step = 0;

  var dispatch_target = new EventTarget();
  dispatch_target.addEventListener("dispatch-order", eventtarget_dispatch_order_dispatch_capture, {
    capture: true,
  });
  dispatch_target.addEventListener("dispatch-order", eventtarget_dispatch_order_dispatch_bubble);
  dispatch_target.dispatchEvent(new Event("dispatch-order"));

  assert_equals(
    eventtarget_dispatch_order_dispatch_step,
    2,
    "expected both capture and bubble listeners to run"
  );
}

test(
  eventtarget_dispatch_order_at_target_test,
  "capture listeners run before bubble listeners at the target"
);

// --- removeEventListener ---
var eventtarget_dispatch_order_removed_called = false;

function eventtarget_dispatch_order_removed_cb(_e) {
  eventtarget_dispatch_order_removed_called = true;
}

function eventtarget_dispatch_order_remove_event_listener_test() {
  eventtarget_dispatch_order_removed_called = false;

  var remove_target = new EventTarget();
  remove_target.addEventListener("remove-listener", eventtarget_dispatch_order_removed_cb);
  remove_target.removeEventListener("remove-listener", eventtarget_dispatch_order_removed_cb);
  remove_target.dispatchEvent(new Event("remove-listener"));

  assert_false(
    eventtarget_dispatch_order_removed_called,
    "removed listener should not be invoked"
  );
}

test(
  eventtarget_dispatch_order_remove_event_listener_test,
  "removeEventListener prevents the listener from firing"
);

// --- once ---
var eventtarget_dispatch_order_once_called = false;

function eventtarget_dispatch_order_once_cb(_e) {
  if (eventtarget_dispatch_order_once_called === true) {
    assert_unreached("once listener ran more than once");
  }
  eventtarget_dispatch_order_once_called = true;
}

function eventtarget_dispatch_order_once_test() {
  eventtarget_dispatch_order_once_called = false;

  var once_target = new EventTarget();
  once_target.addEventListener("once", eventtarget_dispatch_order_once_cb, { once: true });
  once_target.dispatchEvent(new Event("once"));
  once_target.dispatchEvent(new Event("once"));

  assert_true(eventtarget_dispatch_order_once_called, "once listener did not run");
}

test(eventtarget_dispatch_order_once_test, "once listeners run at most once");

// --- passive ---
var eventtarget_dispatch_order_passive_called = false;
var eventtarget_dispatch_order_passive_event = null;

function eventtarget_dispatch_order_passive_cb(e) {
  eventtarget_dispatch_order_passive_called = true;
  eventtarget_dispatch_order_passive_event = e;
  e.preventDefault();
}

function eventtarget_dispatch_order_passive_test() {
  eventtarget_dispatch_order_passive_called = false;
  eventtarget_dispatch_order_passive_event = null;

  var passive_target = new EventTarget();
  passive_target.addEventListener("passive", eventtarget_dispatch_order_passive_cb, {
    passive: true,
  });
  var ev = new Event("passive", { cancelable: true });
  var ok = passive_target.dispatchEvent(ev);

  assert_true(eventtarget_dispatch_order_passive_called, "passive listener was not invoked");
  assert_true(ok, "dispatchEvent should return true when preventDefault was ignored");
  assert_false(ev.defaultPrevented, "preventDefault() in passive listener must be ignored");
  assert_equals(
    eventtarget_dispatch_order_passive_event,
    ev,
    "listener should receive the same Event object passed to dispatchEvent"
  );
}

test(
  eventtarget_dispatch_order_passive_test,
  "passive listeners ignore preventDefault()"
);
