// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop test: `setInterval` callbacks run as tasks, and `clearInterval` cancels the
// timer.

var interval_fired = false;
var interval_fired_more_than_once = false;
var interval_id = 0;

function interval_apis_exist_test() {
  setInterval;
  clearInterval;
  setTimeout;
}

test(interval_apis_exist_test, "setInterval/clearInterval APIs exist");

function interval_tick() {
  if (interval_fired) {
    interval_fired_more_than_once = true;
    return;
  }
  interval_fired = true;
  clearInterval(interval_id);
}

function finish_interval_cancel_test() {
  // Clean up in case the timer did not get cancelled.
  clearInterval(interval_id);

  if (interval_fired !== true) {
    throw "interval did not fire";
  }
  if (interval_fired_more_than_once) {
    throw "interval fired more than once";
  }
}

function run_interval_cancel_test(t) {
  interval_fired = false;
  interval_fired_more_than_once = false;
  interval_id = setInterval(interval_tick, 0);
  setTimeout(t.step_func_done(finish_interval_cancel_test), 20);
}

async_test(run_interval_cancel_test, "clearInterval cancels an interval timer");
