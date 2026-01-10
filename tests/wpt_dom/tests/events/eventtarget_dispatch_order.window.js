// META: script=/resources/testharness.js
//
// Curated `EventTarget` semantics checks expressed as testharness subtests.

test(() => {
  var dispatch_step = 0;

  function dispatch_capture(_e) {
    assert_equals(dispatch_step, 0, "capture listener ran out of order");
    dispatch_step = 1;
  }

  function dispatch_bubble(_e) {
    assert_equals(dispatch_step, 1, "bubble listener ran out of order");
    dispatch_step = 2;
  }

  var target = new EventTarget();
  target.addEventListener("dispatch-order", dispatch_capture, { capture: true });
  target.addEventListener("dispatch-order", dispatch_bubble);
  target.dispatchEvent(new Event("dispatch-order"));

  assert_equals(dispatch_step, 2, "expected both capture and bubble listeners to run");
}, "dispatch order at target: capture listeners fire before bubble listeners");

test(() => {
  var called = false;

  function removed_cb(_e) {
    called = true;
  }

  var target = new EventTarget();
  target.addEventListener("remove-listener", removed_cb);
  target.removeEventListener("remove-listener", removed_cb);
  target.dispatchEvent(new Event("remove-listener"));

  assert_false(called, "removed listener should not be invoked");
}, "removeEventListener removes a listener");

test(() => {
  var called = 0;

  function once_cb(_e) {
    if (called === 0) {
      called = 1;
    } else if (called === 1) {
      called = 2;
    } else {
      called = 3;
    }
  }

  var target = new EventTarget();
  target.addEventListener("once", once_cb, { once: true });
  target.dispatchEvent(new Event("once"));
  target.dispatchEvent(new Event("once"));

  assert_equals(called, 1, "once listener should run exactly once");
}, "once option runs a listener at most once");

test(() => {
  var called = false;
  var received = null;

  function passive_cb(e) {
    called = true;
    received = e;
    e.preventDefault();
  }

  var target = new EventTarget();
  target.addEventListener("passive", passive_cb, { passive: true });

  var ev = new Event("passive", { cancelable: true });
  var ok = target.dispatchEvent(ev);

  assert_true(called, "passive listener was not invoked");
  assert_true(ok, "dispatchEvent should return true when preventDefault was ignored");
  assert_false(ev.defaultPrevented, "preventDefault() in passive listener must be ignored");
  assert_equals(received, ev, "listener should receive the same Event object passed to dispatchEvent");
}, "passive option ignores preventDefault() and does not cancel dispatchEvent");
