// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Minimal async-delivery test for `IntersectionObserver` under the offline WPT DOM runner.

promise_test(
  async () => {
    const target = document.createElement("div");
    document.body.appendChild(target);

    let called = false;
    let entryCount = -1;

    const obs = new IntersectionObserver((entries) => {
      called = true;
      entryCount = entries.length;
    });
    obs.observe(target);

    assert_false(
      called,
      "IntersectionObserver callback should not be invoked synchronously by observe()"
    );

    // The offline runner drives deterministic microtask checkpoints; `observe()` should schedule at
    // least one microtask delivery even without renderer/layout.
    await Promise.resolve();

    assert_true(
      called,
      "IntersectionObserver callback should be invoked asynchronously after observe()"
    );
    assert_true(entryCount > 0, "IntersectionObserver should deliver at least one entry");
  },
  "IntersectionObserver fires asynchronously after observe()"
);

