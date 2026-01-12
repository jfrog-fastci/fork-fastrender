// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Lifecycle: ensure the standard event constructors exist and expose their init dictionaries.
//
// This is a focused offline WPT-ish test covering js_html_integration lifecycle plumbing:
// - BeforeUnloadEvent (returnValue)
// - PageTransitionEvent (persisted)

test(() => {
  assert_equals(typeof BeforeUnloadEvent, "function");
}, "BeforeUnloadEvent constructor exists");

test(() => {
  assert_equals(typeof PageTransitionEvent, "function");
}, "PageTransitionEvent constructor exists");

test(() => {
  const e = new BeforeUnloadEvent("beforeunload", { returnValue: "bye", cancelable: true });
  assert_true(e instanceof BeforeUnloadEvent);
  assert_equals(e.type, "beforeunload");
  assert_equals(e.returnValue, "bye");
  e.returnValue = "changed";
  assert_equals(e.returnValue, "changed");
}, "BeforeUnloadEvent returnValue init/assignment roundtrips");

test(() => {
  const e = new PageTransitionEvent("pageshow", { persisted: true, bubbles: true });
  assert_true(e instanceof PageTransitionEvent);
  assert_equals(e.type, "pageshow");
  assert_equals(e.persisted, true);
  assert_equals(e.bubbles, true);
}, "PageTransitionEvent persisted init works");

test(() => {
  const e = new PageTransitionEvent("pagehide", { persisted: false, cancelable: true });
  assert_true(e instanceof PageTransitionEvent);
  assert_equals(e.type, "pagehide");
  assert_equals(e.persisted, false);
  assert_equals(e.cancelable, true);
}, "PageTransitionEvent supports pagehide + persisted=false");
