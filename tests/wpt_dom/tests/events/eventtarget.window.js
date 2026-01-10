// META: script=/resources/testharness.js

// Curated DOM EventTarget semantics checks. This file intentionally uses conservative JS syntax so
// it can run on the minimal vm-js backend.

// --- capture/bubble ordering ---
var eventtarget_capture_bubble_order_step = 0;

function eventtarget_capture_bubble_parent_capture(_e) {
  assert_equals(eventtarget_capture_bubble_order_step, 0, "parent capture ran out of order");
  eventtarget_capture_bubble_order_step = 1;
}

function eventtarget_capture_bubble_child_capture(_e) {
  assert_equals(eventtarget_capture_bubble_order_step, 1, "child capture ran out of order");
  eventtarget_capture_bubble_order_step = 2;
}

function eventtarget_capture_bubble_child_bubble(_e) {
  assert_equals(eventtarget_capture_bubble_order_step, 2, "child bubble ran out of order");
  eventtarget_capture_bubble_order_step = 3;
}

function eventtarget_capture_bubble_parent_bubble(_e) {
  assert_equals(eventtarget_capture_bubble_order_step, 3, "parent bubble ran out of order");
  eventtarget_capture_bubble_order_step = 4;
}

function eventtarget_capture_bubble_order_test() {
  eventtarget_capture_bubble_order_step = 0;

  var parent = document.createElement("div");
  var child = document.createElement("span");
  parent.appendChild(child);

  parent.addEventListener(
    "capture-bubble-order",
    eventtarget_capture_bubble_parent_capture,
    true
  );
  parent.addEventListener("capture-bubble-order", eventtarget_capture_bubble_parent_bubble);

  child.addEventListener("capture-bubble-order", eventtarget_capture_bubble_child_capture, {
    capture: true,
  });
  child.addEventListener("capture-bubble-order", eventtarget_capture_bubble_child_bubble);

  var ok = child.dispatchEvent(new Event("capture-bubble-order", { bubbles: true }));
  assert_true(ok, "dispatchEvent should return true when not canceled");
  assert_equals(
    eventtarget_capture_bubble_order_step,
    4,
    "expected capture listeners to run before bubbling listeners"
  );
}

test(
  eventtarget_capture_bubble_order_test,
  "capture/bubble ordering on a parent/child element subtree"
);

// --- preventDefault + cancelable ---
var eventtarget_prevent_saw = false;

function eventtarget_prevent_listener(e) {
  eventtarget_prevent_saw = true;
  e.preventDefault();
}

function eventtarget_prevent_default_cancelable_test() {
  eventtarget_prevent_saw = false;

  var el = document.createElement("div");
  el.addEventListener("prevent-default", eventtarget_prevent_listener);

  var ev = new Event("prevent-default", { cancelable: true });
  var ok = el.dispatchEvent(ev);

  assert_true(eventtarget_prevent_saw, "listener should have run");
  assert_true(
    ev.defaultPrevented,
    "preventDefault should set defaultPrevented for cancelable events"
  );
  assert_false(ok, "dispatchEvent should return false when default was prevented");
}

test(
  eventtarget_prevent_default_cancelable_test,
  "preventDefault() on cancelable events sets defaultPrevented and changes dispatchEvent return"
);

// --- removeEventListener ---
var eventtarget_removed_called = false;

function eventtarget_removed_cb(_e) {
  eventtarget_removed_called = true;
}

function eventtarget_remove_event_listener_test() {
  eventtarget_removed_called = false;

  var el = document.createElement("div");
  el.addEventListener("remove-listener", eventtarget_removed_cb);
  el.removeEventListener("remove-listener", eventtarget_removed_cb);
  el.dispatchEvent(new Event("remove-listener"));

  assert_false(eventtarget_removed_called, "removed listener should not be invoked");
}

test(eventtarget_remove_event_listener_test, "removeEventListener removes a listener");

// --- addEventListener ignores duplicates ---
var eventtarget_dup_called = false;

function eventtarget_dup_cb(_e) {
  if (eventtarget_dup_called === true) {
    assert_unreached("duplicate addEventListener registrations should be ignored");
  }
  eventtarget_dup_called = true;
}

function eventtarget_add_event_listener_ignores_duplicates_test() {
  eventtarget_dup_called = false;

  var el = document.createElement("div");
  el.addEventListener("duplicate-listener", eventtarget_dup_cb);
  el.addEventListener("duplicate-listener", eventtarget_dup_cb);
  el.dispatchEvent(new Event("duplicate-listener"));

  assert_true(eventtarget_dup_called, "listener should have been invoked once");
}

test(
  eventtarget_add_event_listener_ignores_duplicates_test,
  "addEventListener ignores duplicate listener registrations"
);
