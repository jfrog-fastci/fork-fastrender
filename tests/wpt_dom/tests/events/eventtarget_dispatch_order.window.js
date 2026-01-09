// META: script=/resources/testharness.js
// META: script=/resources/eventtarget_shim.js
// META: script=/resources/fastrender_testharness_report.js

test(function () {
  var log = "";
  var et = new EventTarget();

  et.addEventListener("x", function () {
    log += "c";
  }, { capture: true });

  et.addEventListener("x", function () {
    log += "b";
  });

  et.dispatchEvent(new Event("x"));
  assert_equals(log, "cb");
}, "EventTarget dispatch order at target: capture listeners before bubble listeners");

test(function () {
  var et = new EventTarget();
  var count = 0;

  function cb() {
    count += 1;
  }

  et.addEventListener("x", cb);
  et.removeEventListener("x", cb);
  et.dispatchEvent(new Event("x"));

  assert_equals(count, 0);
}, "removeEventListener removes listeners");

test(function () {
  var et = new EventTarget();
  var count = 0;

  function cb() {
    count += 1;
  }

  et.addEventListener("x", cb, { once: true });
  et.dispatchEvent(new Event("x"));
  et.dispatchEvent(new Event("x"));

  assert_equals(count, 1);
}, "once: listener runs only once");

test(function () {
  var et = new EventTarget();
  var called = false;

  et.addEventListener(
    "x",
    function (e) {
      called = true;
      e.preventDefault();
    },
    { passive: true }
  );

  var ev = new Event("x", { cancelable: true });
  var ok = et.dispatchEvent(ev);

  assert_true(called);
  assert_true(ok);
  assert_false(ev.defaultPrevented);
}, "passive: preventDefault() is ignored");

