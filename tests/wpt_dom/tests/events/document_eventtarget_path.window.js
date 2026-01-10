// META: script=/resources/testharness.js
//
// Curated DOM EventTarget propagation checks using the host `document` + `createElement` shims.
// This file intentionally uses conservative JS syntax so it can run on the minimal vm-js backend.

var document_eventtarget_path_order_step = 0;
var document_eventtarget_path_parent = null;
var document_eventtarget_path_child = null;

function document_eventtarget_path_win_capture(e) {
  assert_equals(document_eventtarget_path_order_step, 0, "window capture ran out of order");
  assert_equals(
    e.target,
    document_eventtarget_path_child,
    "event.target should be the dispatch target"
  );
  assert_equals(e.currentTarget, window, "window currentTarget in capture");
  document_eventtarget_path_order_step = 1;
}

function document_eventtarget_path_doc_capture(e) {
  assert_equals(document_eventtarget_path_order_step, 1, "document capture ran out of order");
  assert_equals(e.currentTarget, document, "document currentTarget in capture");
  document_eventtarget_path_order_step = 2;
}

function document_eventtarget_path_parent_capture(e) {
  assert_equals(document_eventtarget_path_order_step, 2, "parent capture ran out of order");
  assert_equals(
    e.currentTarget,
    document_eventtarget_path_parent,
    "parent currentTarget in capture"
  );
  document_eventtarget_path_order_step = 3;
}

function document_eventtarget_path_child_capture(e) {
  assert_equals(document_eventtarget_path_order_step, 3, "child capture ran out of order");
  assert_equals(
    e.currentTarget,
    document_eventtarget_path_child,
    "child currentTarget in capture"
  );
  document_eventtarget_path_order_step = 4;
}

function document_eventtarget_path_child_bubble(e) {
  assert_equals(document_eventtarget_path_order_step, 4, "child bubble ran out of order");
  assert_equals(
    e.currentTarget,
    document_eventtarget_path_child,
    "child currentTarget in bubble"
  );
  document_eventtarget_path_order_step = 5;
}

function document_eventtarget_path_parent_bubble(e) {
  assert_equals(document_eventtarget_path_order_step, 5, "parent bubble ran out of order");
  assert_equals(
    e.currentTarget,
    document_eventtarget_path_parent,
    "parent currentTarget in bubble"
  );
  document_eventtarget_path_order_step = 6;
}

function document_eventtarget_path_doc_bubble(e) {
  assert_equals(document_eventtarget_path_order_step, 6, "document bubble ran out of order");
  assert_equals(e.currentTarget, document, "document currentTarget in bubble");
  document_eventtarget_path_order_step = 7;
}

function document_eventtarget_path_win_bubble(e) {
  assert_equals(document_eventtarget_path_order_step, 7, "window bubble ran out of order");
  assert_equals(e.currentTarget, window, "window currentTarget in bubble");
  document_eventtarget_path_order_step = 8;
}

function document_eventtarget_path_window_document_path_order_test() {
  document_eventtarget_path_order_step = 0;

  var parent = document.createElement("div");
  var child = document.createElement("span");
  document_eventtarget_path_parent = parent;
  document_eventtarget_path_child = child;

  // Attach the subtree so the propagation path includes window + document.
  document.body.appendChild(parent);
  parent.appendChild(child);

  window.addEventListener("document-path", document_eventtarget_path_win_capture, true);
  document.addEventListener("document-path", document_eventtarget_path_doc_capture, true);
  parent.addEventListener("document-path", document_eventtarget_path_parent_capture, true);
  child.addEventListener("document-path", document_eventtarget_path_child_capture, true);

  child.addEventListener("document-path", document_eventtarget_path_child_bubble);
  parent.addEventListener("document-path", document_eventtarget_path_parent_bubble);
  document.addEventListener("document-path", document_eventtarget_path_doc_bubble);
  window.addEventListener("document-path", document_eventtarget_path_win_bubble);

  var ok = child.dispatchEvent(new Event("document-path", { bubbles: true }));
  assert_true(ok, "dispatchEvent should return true when not canceled");
  assert_equals(
    document_eventtarget_path_order_step,
    8,
    "expected all capture/target/bubble listeners to run"
  );
}

test(
  document_eventtarget_path_window_document_path_order_test,
  "DOM event propagation path includes Document and Window"
);
