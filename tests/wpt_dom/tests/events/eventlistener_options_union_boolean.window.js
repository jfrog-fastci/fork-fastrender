// META: script=/resources/testharness.js
//
// Curated DOM EventTarget "options union" semantics checks. This file intentionally uses
// conservative JS syntax so it can run on the minimal vm-js backend.
//
// Per WHATWG DOM + WebIDL union conversion semantics:
// - If the `options` value is an object, it is treated as a dictionary.
// - Otherwise, it is treated as a boolean and `ToBoolean(options)` is used for the `capture` flag.

// --- addEventListener(options: 1) registers a capture listener ---
var eventlistener_options_union_capture_called = false;

function eventlistener_options_union_capture_listener(e) {
  eventlistener_options_union_capture_called = true;
  assert_equals(e.eventPhase, 1, "listener should run during capture (CAPTURING_PHASE)");
}

function eventlistener_options_union_add_event_listener_test() {
  eventlistener_options_union_capture_called = false;

  var parent = document.createElement("div");
  var child = document.createElement("span");
  parent.appendChild(child);

  parent.addEventListener(
    "eventlistener-options-union-add",
    eventlistener_options_union_capture_listener,
    1
  );

  child.dispatchEvent(new Event("eventlistener-options-union-add", { bubbles: true }));

  assert_true(
    eventlistener_options_union_capture_called,
    "capture listener registered with options=1 should run"
  );
}

test(
  eventlistener_options_union_add_event_listener_test,
  "addEventListener(type, cb, 1) registers a capture listener via boolean union conversion"
);

// --- removeEventListener(options: 1) removes a capture listener ---
var eventlistener_options_union_removed_called = false;

function eventlistener_options_union_removed_listener(_e) {
  eventlistener_options_union_removed_called = true;
}

function eventlistener_options_union_remove_event_listener_test() {
  eventlistener_options_union_removed_called = false;

  var parent = document.createElement("div");
  var child = document.createElement("span");
  parent.appendChild(child);

  parent.addEventListener(
    "eventlistener-options-union-remove",
    eventlistener_options_union_removed_listener,
    1
  );
  parent.removeEventListener(
    "eventlistener-options-union-remove",
    eventlistener_options_union_removed_listener,
    1
  );

  child.dispatchEvent(new Event("eventlistener-options-union-remove", { bubbles: true }));

  assert_false(
    eventlistener_options_union_removed_called,
    "removeEventListener(type, cb, 1) should remove the capture listener registered with 1"
  );
}

test(
  eventlistener_options_union_remove_event_listener_test,
  "removeEventListener(type, cb, 1) removes capture listener via boolean union conversion"
);
