// META: script=/resources/testharness.js

// Curated DOM `EventTarget` semantics checks expressed as testharness subtests.

test(() => {
  var order_step = 0;

  function parent_capture(_e) {
    assert_equals(order_step, 0, "parent capture ran out of order");
    order_step = 1;
  }

  function child_capture(_e) {
    assert_equals(order_step, 1, "child capture ran out of order");
    order_step = 2;
  }

  function child_bubble(_e) {
    assert_equals(order_step, 2, "child bubble ran out of order");
    order_step = 3;
  }

  function parent_bubble(_e) {
    assert_equals(order_step, 3, "parent bubble ran out of order");
    order_step = 4;
  }

  var parent = document.createElement("div");
  var child = document.createElement("span");
  parent.appendChild(child);

  parent.addEventListener("capture-bubble-order", parent_capture, true);
  parent.addEventListener("capture-bubble-order", parent_bubble);

  child.addEventListener("capture-bubble-order", child_capture, { capture: true });
  child.addEventListener("capture-bubble-order", child_bubble);

  var ok = child.dispatchEvent(new Event("capture-bubble-order", { bubbles: true }));
  assert_true(ok, "dispatchEvent should return true when not canceled");
  assert_equals(order_step, 4, "expected capture listeners to run before bubbling listeners");
}, "capture/bubble ordering on a parent/child element subtree");

test(() => {
  var saw = false;

  function prevent_listener(e) {
    saw = true;
    e.preventDefault();
  }

  var el = document.createElement("div");
  el.addEventListener("prevent-default", prevent_listener);

  var ev = new Event("prevent-default", { cancelable: true });
  var ok = el.dispatchEvent(ev);

  assert_true(saw, "listener should have run");
  assert_true(ev.defaultPrevented, "preventDefault should set defaultPrevented for cancelable events");
  assert_false(ok, "dispatchEvent should return false when default was prevented");
}, "preventDefault() on cancelable events sets defaultPrevented and changes dispatchEvent return");

test(() => {
  var called = false;

  function removed_cb(_e) {
    called = true;
  }

  var el = document.createElement("div");
  el.addEventListener("remove-listener", removed_cb);
  el.removeEventListener("remove-listener", removed_cb);
  el.dispatchEvent(new Event("remove-listener"));

  assert_false(called, "removed listener should not be invoked");
}, "removeEventListener removes a listener");

test(() => {
  var called = 0;

  function dup_cb(_e) {
    if (called === 0) {
      called = 1;
    } else if (called === 1) {
      called = 2;
    } else {
      called = 3;
    }
  }

  var el = document.createElement("div");
  el.addEventListener("duplicate-listener", dup_cb);
  el.addEventListener("duplicate-listener", dup_cb);
  el.dispatchEvent(new Event("duplicate-listener"));

  assert_equals(called, 1, "duplicate addEventListener registrations should be ignored");
}, "addEventListener ignores duplicate listener registrations");
