// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Regression test: ensure MutationObserver delivery works for DOM mutations that go through
// FastRender's vm-js DomHostVmJs "fast-path" shims (dataset/classList/style), and that no-op writes
// do not schedule extra deliveries.

promise_test(
  async () => {
    const target = document.createElement("div");
    document.body.appendChild(target);

    const calls = [];
    const lens = [];
    const attrs = [];

    const obs = new MutationObserver((records) => {
      calls.push(true);
      lens.push(records.length);
      // Avoid `for`/`++` loops to keep this test compatible with minimal JS backends.
      attrs.push(records[0] ? records[0].attributeName : null);
      attrs.push(records[1] ? records[1].attributeName : null);
      attrs.push(records[2] ? records[2].attributeName : null);
      attrs.push(records[3] ? records[3].attributeName : null);
    });
    obs.observe(target, { attributes: true });

    // Each of these uses the vm-js DOM shim fast-path when running under a real WindowHost.
    target.dataset.foo = "a";
    target.classList.add("x");
    target.style.setProperty("color", "red");
    target.style.width = "1px";

    // MutationObserver notifications are delivered as microtasks.
    await Promise.resolve();

    assert_equals(calls.length, 1);
    assert_equals(lens[0], 4);
    assert_equals(attrs[0], "data-foo");
    assert_equals(attrs[1], "class");
    assert_equals(attrs[2], "style");
    assert_equals(attrs[3], "style");

    // No-op writes must not enqueue additional MutationObserver deliveries.
    target.dataset.foo = "a";
    target.classList.add("x");
    target.style.setProperty("color", "red");
    target.style.width = "1px";

    await Promise.resolve();
    assert_equals(calls.length, 1);
  },
  "MutationObserver delivers for dataset/classList/style fast-path mutations and not for no-op writes"
);
