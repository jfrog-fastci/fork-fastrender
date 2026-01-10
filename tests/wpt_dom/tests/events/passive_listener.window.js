// META: script=/resources/testharness.js
//
// Curated passive listener check.

var passive_listener_window_ran = false;

function passive_listener_window_listener(e) {
  passive_listener_window_ran = true;
  e.preventDefault();
}

function passive_listener_window_passive_listener_test() {
  passive_listener_window_ran = false;

  var target = new EventTarget();
  target.addEventListener("passive", passive_listener_window_listener, { passive: true });

  var ev = new Event("passive", { cancelable: true });
  var res = target.dispatchEvent(ev);

  assert_true(passive_listener_window_ran, "passive listener did not run");
  assert_false(ev.defaultPrevented, "preventDefault must be ignored in passive listeners");
  assert_true(res, "dispatchEvent must return true when default was not prevented");
}

test(
  passive_listener_window_passive_listener_test,
  "passive listeners ignore preventDefault() and do not affect dispatchEvent()"
);
