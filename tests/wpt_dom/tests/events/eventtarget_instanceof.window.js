// META: script=/resources/testharness.js
//
// Ensure DOM wrappers that implement EventTarget participate in the shared EventTarget prototype
// chain so `instanceof EventTarget` works as in browsers.

test(function () {
  var el = document.createElement("div");

  assert_true(el instanceof EventTarget, "Element should be instanceof EventTarget");
  assert_true(document instanceof EventTarget, "Document should be instanceof EventTarget");
  assert_true(new EventTarget() instanceof EventTarget, "new EventTarget() should be instanceof EventTarget");
}, "DOM wrappers and new EventTarget() satisfy instanceof EventTarget");

