// META: script=/resources/testharness.js
//
// Curated passive listener check expressed as a testharness subtest.

test(() => {
  var target = new EventTarget();
  var ran = false;

  function listener(e) {
    ran = true;
    e.preventDefault();
  }

  target.addEventListener("passive", listener, { passive: true });

  var ev = new Event("passive", { cancelable: true });
  var res = target.dispatchEvent(ev);

  assert_true(ran, "passive listener did not run");
  assert_false(ev.defaultPrevented, "preventDefault must be ignored in passive listeners");
  assert_true(res, "dispatchEvent must return true when default was not prevented");
}, "passive listeners ignore preventDefault()");
