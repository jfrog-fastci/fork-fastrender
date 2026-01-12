// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Minimal async-delivery test for `ResizeObserver` under the offline WPT DOM runner.

promise_test(
  async () => {
    const target = document.createElement("div");
    document.body.appendChild(target);

    let called = false;
    let delivered = null;

    const obs = new ResizeObserver((entries) => {
      called = true;
      delivered = entries;
    });
    obs.observe(target);

    assert_false(
      called,
      "ResizeObserver callback should not be invoked synchronously by observe()"
    );

    // The offline runner drives deterministic microtask checkpoints; `observe()` should schedule at
    // least one microtask delivery even without renderer/layout.
    await Promise.resolve();

    assert_true(called, "ResizeObserver callback should be invoked asynchronously after observe()");
    assert_true(delivered && delivered.length > 0, "ResizeObserver should deliver at least one entry");
    assert_equals(delivered[0].target, target);
    assert_equals(delivered[0].contentRect.width, 0);
    assert_equals(delivered[0].contentRect.height, 0);
  },
  "ResizeObserver fires asynchronously after observe()"
);

