// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Lifecycle: visibilitychange event + EventHandler attribute semantics.

async_test((t) => {
  let add_event_listener_count = 0;
  let onvisibilitychange_count = 0;
  let onvisibilitychange_ok = true;

  document.onvisibilitychange = function (e) {
    onvisibilitychange_count++;
    onvisibilitychange_ok =
      onvisibilitychange_ok &&
      this === document &&
      e &&
      e.type === "visibilitychange" &&
      e.target === document &&
      e.currentTarget === document &&
      e.eventPhase === 2;
  };

  document.addEventListener("visibilitychange", () => {
    add_event_listener_count++;
  });

  document.dispatchEvent(new Event("visibilitychange"));

  t.step_timeout(() => {
    assert_equals(onvisibilitychange_count, 1, "onvisibilitychange should be called once");
    assert_true(onvisibilitychange_ok, "onvisibilitychange should see correct this/event fields");
    assert_equals(add_event_listener_count, 1, "addEventListener handler should be called once");
    t.done();
  }, 0);
}, "document.onvisibilitychange runs on visibilitychange event dispatch");

