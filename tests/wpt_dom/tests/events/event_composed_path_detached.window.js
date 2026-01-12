// META: script=/resources/testharness.js
//
// `composedPath()` on a detached element should not include document/window.

function event_composed_path_detached_test() {
  var el = document.createElement("div");
  var ev = new Event("x");
  el.dispatchEvent(ev);

  var path = ev.composedPath();
  assert_equals(path.length, 1, "detached element path should only include the target");
  assert_equals(path[0], el, "detached element path should be [target]");
}

test(event_composed_path_detached_test, "Event.prototype.composedPath on detached elements");

