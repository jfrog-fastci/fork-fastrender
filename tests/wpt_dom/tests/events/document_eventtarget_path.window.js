// META: script=/resources/testharness.js
//
// Curated DOM EventTarget propagation checks expressed as testharness subtests.

test(() => {
  var order_step = 0;

  var parent = document.createElement("div");
  var child = document.createElement("span");

  // Attach the subtree so the propagation path includes window + document.
  document.appendChild(parent);
  parent.appendChild(child);

  function win_capture(e) {
    assert_equals(order_step, 0, "window capture ran out of order");
    assert_equals(e.target, child, "event.target should be the dispatch target");
    assert_equals(e.currentTarget, window, "window currentTarget in capture");
    order_step = 1;
  }

  function doc_capture(e) {
    assert_equals(order_step, 1, "document capture ran out of order");
    assert_equals(e.currentTarget, document, "document currentTarget in capture");
    order_step = 2;
  }

  function parent_capture(e) {
    assert_equals(order_step, 2, "parent capture ran out of order");
    assert_equals(e.currentTarget, parent, "parent currentTarget in capture");
    order_step = 3;
  }

  function child_capture(e) {
    assert_equals(order_step, 3, "child capture ran out of order");
    assert_equals(e.currentTarget, child, "child currentTarget in capture");
    order_step = 4;
  }

  function child_bubble(e) {
    assert_equals(order_step, 4, "child bubble ran out of order");
    assert_equals(e.currentTarget, child, "child currentTarget in bubble");
    order_step = 5;
  }

  function parent_bubble(e) {
    assert_equals(order_step, 5, "parent bubble ran out of order");
    assert_equals(e.currentTarget, parent, "parent currentTarget in bubble");
    order_step = 6;
  }

  function doc_bubble(e) {
    assert_equals(order_step, 6, "document bubble ran out of order");
    assert_equals(e.currentTarget, document, "document currentTarget in bubble");
    order_step = 7;
  }

  function win_bubble(e) {
    assert_equals(order_step, 7, "window bubble ran out of order");
    assert_equals(e.currentTarget, window, "window currentTarget in bubble");
    order_step = 8;
  }

  window.addEventListener("document-path", win_capture, true);
  document.addEventListener("document-path", doc_capture, true);
  parent.addEventListener("document-path", parent_capture, true);
  child.addEventListener("document-path", child_capture, true);

  child.addEventListener("document-path", child_bubble);
  parent.addEventListener("document-path", parent_bubble);
  document.addEventListener("document-path", doc_bubble);
  window.addEventListener("document-path", win_bubble);

  var ok = child.dispatchEvent(new Event("document-path", { bubbles: true }));
  assert_true(ok, "dispatchEvent should return true when not canceled");
  assert_equals(order_step, 8, "expected all capture/target/bubble listeners to run");
}, "DOM event propagation path includes Document and Window");
