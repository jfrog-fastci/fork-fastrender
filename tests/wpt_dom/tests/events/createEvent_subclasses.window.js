// META: script=/resources/testharness.js
//
// document.createEvent support for common Event subclasses.

test(function () {
  var ev = document.createEvent("MouseEvent");
  assert_true(ev instanceof MouseEvent, "createEvent(MouseEvent) instanceof MouseEvent");
  assert_true(ev instanceof UIEvent, "createEvent(MouseEvent) instanceof UIEvent");
  assert_true(ev instanceof Event, "createEvent(MouseEvent) instanceof Event");

  var t = new EventTarget();
  assert_throws_dom("InvalidStateError", function () {
    t.dispatchEvent(ev);
  });

  // Minimal initMouseEvent: type/bubbles/cancelable/view/detail + coordinates.
  ev.initMouseEvent("click", true, true, window, 0, 1, 2, 3, 4);

  var saw = false;
  t.addEventListener("click", function (e) {
    saw = true;
    assert_equals(e, ev, "listener should receive the same Event object");
  });
  var ok = t.dispatchEvent(ev);
  assert_true(saw, "event listener should have run");
  assert_true(ok, "dispatchEvent should return true when not canceled");
}, "createEvent('MouseEvent') returns MouseEvent and requires init before dispatch");

test(function () {
  // Case-insensitive matching for legacy interface names.
  var ev = document.createEvent("mouseevent");
  assert_true(ev instanceof MouseEvent);
}, "createEvent is case-insensitive for subclass names");

