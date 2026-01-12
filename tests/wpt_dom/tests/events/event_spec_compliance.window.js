// META: script=/resources/testharness.js
//
// Curated spec compliance checks for Event/EventTarget. This file intentionally uses conservative
// JS syntax so it can run on the minimal vm-js backend.

// --- constructors require `new` ---
function event_spec_compliance_call_event_constructor() {
  Event("x");
}

function event_spec_compliance_call_custom_event_constructor() {
  CustomEvent("x");
}

function event_spec_compliance_constructors_require_new_test() {
  assert_throws_js(TypeError, event_spec_compliance_call_event_constructor);
  assert_throws_js(TypeError, event_spec_compliance_call_custom_event_constructor);
}

test(event_spec_compliance_constructors_require_new_test, "Event constructors require new");

// --- read-only attributes ---
function event_spec_compliance_event_type_is_read_only_test() {
  var e = new Event("x");
  assert_equals(e.type, "x");
  e.type = "y";
  assert_equals(e.type, "x");
}

test(event_spec_compliance_event_type_is_read_only_test, "Event.type is a read-only attribute");

// --- brand checks / illegal invocation ---
function event_spec_compliance_event_prevent_default_illegal_invocation() {
  Event.prototype.preventDefault.call({});
}

function event_spec_compliance_custom_event_init_illegal_invocation() {
  CustomEvent.prototype.initCustomEvent.call(new Event("x"), "x", false, false, null);
}

function event_spec_compliance_brand_checks_test() {
  assert_throws_js(TypeError, event_spec_compliance_event_prevent_default_illegal_invocation);
  assert_throws_js(TypeError, event_spec_compliance_custom_event_init_illegal_invocation);
}

test(event_spec_compliance_brand_checks_test, "Event methods perform WebIDL brand checks");

// --- instanceof semantics ---
function event_spec_compliance_dom_objects_are_event_targets_test() {
  assert_true(document instanceof EventTarget);
  assert_true(document.createElement("div") instanceof EventTarget);
}

test(
  event_spec_compliance_dom_objects_are_event_targets_test,
  "DOM objects are instanceof EventTarget"
);

// --- dispatchEvent argument validation ---
var event_spec_compliance_dispatch_target = null;

function event_spec_compliance_dispatch_invalid_arg() {
  event_spec_compliance_dispatch_target.dispatchEvent({});
}

function event_spec_compliance_dispatch_event_argument_validation_test() {
  event_spec_compliance_dispatch_target = new EventTarget();
  assert_throws_js(TypeError, event_spec_compliance_dispatch_invalid_arg);
}

test(
  event_spec_compliance_dispatch_event_argument_validation_test,
  "dispatchEvent validates its Event argument"
);

// --- InvalidStateError: legacy events must be initialized ---
var event_spec_compliance_legacy_target = null;
var event_spec_compliance_legacy_event = null;

function event_spec_compliance_dispatch_uninitialized_legacy_event() {
  event_spec_compliance_legacy_target.dispatchEvent(event_spec_compliance_legacy_event);
}

function event_spec_compliance_dispatch_legacy_event_initialized_test() {
  event_spec_compliance_legacy_target = new EventTarget();
  event_spec_compliance_legacy_event = document.createEvent("Event");

  assert_throws_dom("InvalidStateError", event_spec_compliance_dispatch_uninitialized_legacy_event);

  event_spec_compliance_legacy_event.initEvent("x", false, false);
  assert_true(event_spec_compliance_legacy_target.dispatchEvent(event_spec_compliance_legacy_event));
}

test(
  event_spec_compliance_dispatch_legacy_event_initialized_test,
  "dispatchEvent throws InvalidStateError for uninitialized legacy events"
);

// --- InvalidStateError: cannot re-dispatch the same event while dispatching ---
var event_spec_compliance_reentrant_target = null;
var event_spec_compliance_reentrant_event = null;
var event_spec_compliance_reentrant_in_listener = false;
var event_spec_compliance_reentrant_threw = false;
var event_spec_compliance_reentrant_thrown_name = null;

function event_spec_compliance_reentrant_listener() {
  // Avoid unbounded recursion if the implementation incorrectly allows re-dispatching.
  if (event_spec_compliance_reentrant_in_listener === true) return;
  event_spec_compliance_reentrant_in_listener = true;

  event_spec_compliance_reentrant_threw = false;
  event_spec_compliance_reentrant_thrown_name = null;
  try {
    event_spec_compliance_reentrant_target.dispatchEvent(event_spec_compliance_reentrant_event);
  } catch (e) {
    event_spec_compliance_reentrant_threw = true;
    try {
      if (e && typeof e === "object" && typeof e.name === "string") {
        event_spec_compliance_reentrant_thrown_name = e.name;
      }
    } catch (_e2) {}
  }

  event_spec_compliance_reentrant_in_listener = false;
}

function event_spec_compliance_dispatch_reentrant_dispatch_test() {
  event_spec_compliance_reentrant_target = new EventTarget();
  event_spec_compliance_reentrant_event = new Event("re");
  event_spec_compliance_reentrant_in_listener = false;
  event_spec_compliance_reentrant_threw = false;
  event_spec_compliance_reentrant_thrown_name = null;

  event_spec_compliance_reentrant_target.addEventListener(
    "re",
    event_spec_compliance_reentrant_listener
  );

  event_spec_compliance_reentrant_target.dispatchEvent(event_spec_compliance_reentrant_event);
  assert_true(event_spec_compliance_reentrant_threw);
  assert_equals(event_spec_compliance_reentrant_thrown_name, "InvalidStateError");
}

test(
  event_spec_compliance_dispatch_reentrant_dispatch_test,
  "dispatchEvent throws InvalidStateError when dispatching the same event re-entrantly"
);
