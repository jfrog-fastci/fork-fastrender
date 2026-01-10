// META: script=/resources/testharness.js

var fired = false;
var fired_more_than_once = false;
var interval_id = 0;

function tick() {
  if (fired) {
    fired_more_than_once = true;
  }
  fired = true;
  clearInterval(interval_id);
}

async_test((t) => {
  fired = false;
  fired_more_than_once = false;
  interval_id = setInterval(tick, 0);
  setTimeout(
    t.step_func_done(() => {
      clearInterval(interval_id);
      assert_true(fired, "interval should fire");
      assert_false(fired_more_than_once, "interval should fire once and then be cancelled");
    }),
    10
  );
}, "setInterval fires once then cancels");
