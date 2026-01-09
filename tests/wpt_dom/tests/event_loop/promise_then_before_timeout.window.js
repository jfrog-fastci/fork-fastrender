// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: Promise reactions run in the microtask queue and must be
// processed before timers.

async_test(function (t) {
  var log = "";

  Promise.resolve().then(
    t.step_func(function () {
      log += "p";
    })
  );

  setTimeout(
    t.step_func_done(function () {
      log += "t";
      assert_equals(log, "pt");
    }),
    0
  );
}, "Promise.then runs before setTimeout(0)");

