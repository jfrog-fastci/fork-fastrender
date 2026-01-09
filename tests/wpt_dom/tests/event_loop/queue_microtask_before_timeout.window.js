// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: microtasks (queueMicrotask) must run before timers.

async_test(function (t) {
  var log = "";

  queueMicrotask(
    t.step_func(function () {
      log += "m";
    })
  );

  setTimeout(
    t.step_func(function () {
      log += "t";
    }),
    0
  );

  setTimeout(
    t.step_func_done(function () {
      assert_equals(log, "mt");
    }),
    0
  );
}, "queueMicrotask runs before setTimeout(0)");

