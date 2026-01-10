// META: script=/resources/testharness.js

async_test((t) => {
  t.step_func_done(() => {
    assert_true(false, "intentional failure for smoke test");
  })();
}, "intentional failing smoke test");
