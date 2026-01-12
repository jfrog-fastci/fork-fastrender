// META: script=/resources/testharness.js
//
// Regression tests: EventTarget listener options must use full `Get` semantics.
// In particular, option members must be readable via the prototype chain and via accessors.

// --- inherited capture ordering ---
var eventtarget_listener_options_get_capture_step = 0;

function eventtarget_listener_options_get_capture_listener(_e) {
  assert_equals(
    eventtarget_listener_options_get_capture_step,
    0,
    "capture listener should run before bubble listener even if registered later"
  );
  eventtarget_listener_options_get_capture_step = 1;
}

function eventtarget_listener_options_get_bubble_listener(_e) {
  assert_equals(
    eventtarget_listener_options_get_capture_step,
    1,
    "bubble listener ran before capture listener"
  );
  eventtarget_listener_options_get_capture_step = 2;
}

function eventtarget_listener_options_get_inherited_capture_order_test() {
  eventtarget_listener_options_get_capture_step = 0;

  var target = new EventTarget();
  target.addEventListener(
    "listener-options-get-capture-order",
    eventtarget_listener_options_get_bubble_listener
  );

  // `capture` is inherited via the prototype chain.
  var capture_options = Object.create({ capture: true });
  target.addEventListener(
    "listener-options-get-capture-order",
    eventtarget_listener_options_get_capture_listener,
    capture_options
  );

  target.dispatchEvent(new Event("listener-options-get-capture-order", { bubbles: true }));

  assert_equals(
    eventtarget_listener_options_get_capture_step,
    2,
    "expected both listeners to run"
  );
}

test(
  eventtarget_listener_options_get_inherited_capture_order_test,
  "addEventListener: options.capture is read via Get (prototype chain), affecting dispatch order"
);

// --- inherited once (accessor on prototype) ---
var eventtarget_listener_options_get_once_ran = false;
var eventtarget_listener_options_get_once_ran_twice = false;
var eventtarget_listener_options_get_once_getter_called = false;

function eventtarget_listener_options_get_once_listener(_e) {
  if (eventtarget_listener_options_get_once_ran === true) {
    eventtarget_listener_options_get_once_ran_twice = true;
  }
  eventtarget_listener_options_get_once_ran = true;
}

function eventtarget_listener_options_get_inherited_once_test() {
  eventtarget_listener_options_get_once_ran = false;
  eventtarget_listener_options_get_once_ran_twice = false;
  eventtarget_listener_options_get_once_getter_called = false;

  var target = new EventTarget();

  var proto = {};
  Object.defineProperty(proto, "once", {
    get: function () {
      eventtarget_listener_options_get_once_getter_called = true;
      return true;
    },
  });
  var options = Object.create(proto);

  target.addEventListener("listener-options-get-once", eventtarget_listener_options_get_once_listener, options);

  target.dispatchEvent(new Event("listener-options-get-once"));
  target.dispatchEvent(new Event("listener-options-get-once"));

  assert_true(
    eventtarget_listener_options_get_once_getter_called,
    "once getter should be invoked via Get"
  );
  assert_true(eventtarget_listener_options_get_once_ran, "once listener did not run");
  assert_false(
    eventtarget_listener_options_get_once_ran_twice,
    "once listener should not run more than once"
  );
}

test(
  eventtarget_listener_options_get_inherited_once_test,
  "addEventListener: options.once is read via Get (prototype chain/accessor), removing after first dispatch"
);

// --- inherited passive ---
var eventtarget_listener_options_get_passive_ran = false;

function eventtarget_listener_options_get_passive_listener(e) {
  eventtarget_listener_options_get_passive_ran = true;
  e.preventDefault();
}

function eventtarget_listener_options_get_inherited_passive_test() {
  eventtarget_listener_options_get_passive_ran = false;

  var target = new EventTarget();
  var options = Object.create({ passive: true });
  target.addEventListener("listener-options-get-passive", eventtarget_listener_options_get_passive_listener, options);

  var ev = new Event("listener-options-get-passive", { cancelable: true });
  var res = target.dispatchEvent(ev);

  assert_true(eventtarget_listener_options_get_passive_ran, "passive listener did not run");
  assert_false(ev.defaultPrevented, "preventDefault must be ignored in passive listeners");
  assert_true(res, "dispatchEvent must return true when default was not prevented");
}

test(
  eventtarget_listener_options_get_inherited_passive_test,
  "addEventListener: options.passive is read via Get (prototype chain)"
);

// --- removeEventListener inherited capture ---
var eventtarget_listener_options_get_remove_ran = false;
var eventtarget_listener_options_get_remove_ran_twice = false;

function eventtarget_listener_options_get_remove_listener(_e) {
  if (eventtarget_listener_options_get_remove_ran === true) {
    eventtarget_listener_options_get_remove_ran_twice = true;
  }
  eventtarget_listener_options_get_remove_ran = true;
}

function eventtarget_listener_options_get_remove_inherited_capture_test() {
  eventtarget_listener_options_get_remove_ran = false;
  eventtarget_listener_options_get_remove_ran_twice = false;

  var target = new EventTarget();

  // Add a bubble listener...
  target.addEventListener("listener-options-get-remove", eventtarget_listener_options_get_remove_listener);
  // ...and a capture listener using inherited `capture: true` (distinct from bubble).
  target.addEventListener(
    "listener-options-get-remove",
    eventtarget_listener_options_get_remove_listener,
    Object.create({ capture: true })
  );

  // Removing with inherited `capture: true` should remove only the capture listener.
  target.removeEventListener(
    "listener-options-get-remove",
    eventtarget_listener_options_get_remove_listener,
    Object.create({ capture: true })
  );

  target.dispatchEvent(new Event("listener-options-get-remove", { bubbles: true }));

  assert_true(eventtarget_listener_options_get_remove_ran, "expected bubble listener to run");
  assert_false(
    eventtarget_listener_options_get_remove_ran_twice,
    "removeEventListener should use Get for capture (prototype chain) and preserve only the bubble listener"
  );
}

test(
  eventtarget_listener_options_get_remove_inherited_capture_test,
  "removeEventListener: options.capture is read via Get (prototype chain)"
);
